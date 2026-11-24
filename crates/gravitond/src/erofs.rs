//! Shared read-only EROFS image builder.
//!
//! Converts an OCI image's ordered tar layers (gzip, zstd, or raw tar) into an
//! uncompressed EROFS v1 filesystem image under `<support_dir>/images/<key>.erofs`.
//! Attaching that file read-only as `/dev/vdb` lets the guest mount an instant
//! `overlayfs` root (`lowerdir` on `/dev/vdb`, `upperdir` on `/dev/vda`) with zero
//! per-container layer unpacking and shared host page cache across containers.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use graviton_core::paths;
use graviton_image::{BlobStore, Layer};
use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};
use sha2::{Digest, Sha256};

/// Logical block-device size exposed to Virtualization.framework for every `/dev/vdb`.
/// Sparse blocks past the actual EROFS image remain zero-allocated on APFS so every
/// container matches the saved VM snapshot `/dev/vdb` geometry.
pub const EROFS_LOGICAL_BYTES: u64 = 16 << 30;

const BLK_SIZE: usize = 4096;
const EROFS_SUPER_OFFSET: u64 = 1024;
const EROFS_SUPER_MAGIC_V1: u32 = 0xE0F5_E1E2;
const EROFS_BLKSZBITS: u8 = 12;
const EROFS_I_VERSION_EXTENDED: u16 = 0x0001; // bit 0: 64-byte extended inode, datamode FLAT_PLAIN = 0

const S_IFIFO: u16 = 0o010_000;
const S_IFDIR: u16 = 0o040_000;
const S_IFREG: u16 = 0o100_000;
const S_IFLNK: u16 = 0o120_000;
const S_IFSOCK: u16 = 0o140_000;
const S_IFMT: u16 = 0o170_000;

const FT_REG_FILE: u8 = 1;
const FT_DIR: u8 = 2;
const FT_FIFO: u8 = 5;
const FT_SOCK: u8 = 6;
const FT_SYMLINK: u8 = 7;

/// Deterministic cache key for an ordered slice of OCI layers.
pub fn layers_cache_key(layers: &[Layer]) -> String {
    let mut h = Sha256::new();
    h.update(b"erofs-v1\n");
    for l in layers {
        h.update(l.digest.as_bytes());
        h.update(b"\n");
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn key_lock(key: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let mut map = LOCKS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
    Arc::clone(map.entry(key.to_string()).or_insert_with(|| Arc::new(Mutex::new(()))))
}

/// Return `<images_dir>/<key>.erofs`, materializing it atomically from `blobs` if missing.
pub fn ensure_image_disk(blobs: &BlobStore, layers: &[Layer]) -> Result<PathBuf> {
    let key = layers_cache_key(layers);
    let dir = paths::images_dir();
    let path = dir.join(format!("{key}.erofs"));
    if path.is_file() {
        return Ok(path);
    }
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let lock = key_lock(&key);
    let _guard = lock.lock().unwrap();
    if path.is_file() {
        return Ok(path);
    }

    let pid = std::process::id();
    let tmp_stage = dir.join(format!(".{key}.{pid}.stage"));
    let tmp_erofs = dir.join(format!(".{key}.{pid}.erofs.tmp"));
    let _cleanup = TempFilesGuard([tmp_stage.clone(), tmp_erofs.clone()]);

    build_erofs_from_layers(blobs, layers, &tmp_stage, &tmp_erofs)?;
    fs::rename(&tmp_erofs, &path).with_context(|| format!("publishing {}", path.display()))?;
    Ok(path)
}

struct TempFilesGuard([PathBuf; 2]);
impl Drop for TempFilesGuard {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = fs::remove_file(p);
        }
    }
}

#[derive(Clone)]
enum NodeKind {
    Dir { children: BTreeMap<Vec<u8>, usize> },
    File { stage_off: u64, size: u64 },
    Symlink { target: Vec<u8> },
    Special,
}

struct VNode {
    mode: u16,
    uid: u32,
    gid: u32,
    mtime: u64,
    kind: NodeKind,
}

struct Vfs {
    nodes: Vec<VNode>,
    stage: BufWriter<File>,
    stage_pos: u64,
}

impl Vfs {
    fn new(stage_path: &Path) -> Result<Self> {
        let stage = BufWriter::with_capacity(256 * 1024, File::create(stage_path)?);
        let root = VNode {
            mode: S_IFDIR | 0o755,
            uid: 0,
            gid: 0,
            mtime: 0,
            kind: NodeKind::Dir { children: BTreeMap::new() },
        };
        Ok(Self { nodes: vec![root], stage, stage_pos: 0 })
    }

    fn alloc_dir(&mut self, mode: u16, uid: u32, gid: u32, mtime: u64) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(VNode { mode: S_IFDIR | (mode & 0o7777), uid, gid, mtime, kind: NodeKind::Dir { children: BTreeMap::new() } });
        idx
    }

    fn ensure_dir_path(&mut self, components: &[Vec<u8>]) -> usize {
        let mut cur = 0usize;
        for name in components {
            let existing = match &self.nodes[cur].kind {
                NodeKind::Dir { children } => children.get(name).copied(),
                _ => None,
            };
            cur = match existing {
                Some(child_id) if matches!(self.nodes[child_id].kind, NodeKind::Dir { .. }) => child_id,
                Some(_) => {
                    let new_id = self.alloc_dir(0o755, 0, 0, 0);
                    if let NodeKind::Dir { children } = &mut self.nodes[cur].kind {
                        children.insert(name.clone(), new_id);
                    }
                    new_id
                }
                None => {
                    let new_id = self.alloc_dir(0o755, 0, 0, 0);
                    if let NodeKind::Dir { children } = &mut self.nodes[cur].kind {
                        children.insert(name.clone(), new_id);
                    }
                    new_id
                }
            };
        }
        cur
    }

    fn lookup(&self, components: &[Vec<u8>]) -> Option<usize> {
        let mut cur = 0usize;
        for name in components {
            let NodeKind::Dir { children } = &self.nodes[cur].kind else { return None };
            cur = *children.get(name)?;
        }
        Some(cur)
    }

    fn apply_tar_stream<R: Read>(&mut self, reader: R) -> Result<()> {
        let mut archive = tar::Archive::new(reader);
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?;
            let parts = clean_components(&path);
            if parts.is_empty() {
                continue;
            }
            let (parent_parts, leaf) = (&parts[..parts.len() - 1], &parts[parts.len() - 1]);

            if leaf.as_slice() == b".wh..wh..opq" {
                let parent_id = self.ensure_dir_path(parent_parts);
                if let NodeKind::Dir { children } = &mut self.nodes[parent_id].kind {
                    children.clear();
                }
                continue;
            }
            if let Some(target_name) = leaf.strip_prefix(b".wh.") {
                if let Some(parent_id) = self.lookup(parent_parts) {
                    if let NodeKind::Dir { children } = &mut self.nodes[parent_id].kind {
                        children.remove(target_name);
                    }
                }
                continue;
            }

            let kind = entry.header().entry_type();
            if kind.is_character_special() || kind.is_block_special() {
                continue;
            }

            let uid = entry.header().uid().unwrap_or(0) as u32;
            let gid = entry.header().gid().unwrap_or(0) as u32;
            let mtime = entry.header().mtime().unwrap_or(0);
            let default_mode = if kind.is_dir() { 0o755 } else { 0o644 };
            let perm = (entry.header().mode().unwrap_or(default_mode) & 0o7777) as u16;

            let parent_id = self.ensure_dir_path(parent_parts);

            if kind.is_dir() {
                let existing = match &self.nodes[parent_id].kind {
                    NodeKind::Dir { children } => children.get(leaf).copied(),
                    _ => None,
                };
                if let Some(cid) = existing {
                    if matches!(self.nodes[cid].kind, NodeKind::Dir { .. }) {
                        self.nodes[cid].mode = S_IFDIR | perm;
                        self.nodes[cid].uid = uid;
                        self.nodes[cid].gid = gid;
                        self.nodes[cid].mtime = mtime;
                        continue;
                    }
                }
                let nid = self.alloc_dir(perm, uid, gid, mtime);
                if let NodeKind::Dir { children } = &mut self.nodes[parent_id].kind {
                    children.insert(leaf.clone(), nid);
                }
            } else if kind.is_hard_link() {
                let Some(raw_link) = entry.link_name()? else { continue };
                let link_parts = clean_components(&raw_link);
                if let Some(target_id) = self.lookup(&link_parts) {
                    if let NodeKind::Dir { children } = &mut self.nodes[parent_id].kind {
                        children.insert(leaf.clone(), target_id);
                    }
                }
            } else if kind.is_symlink() {
                let target = entry.link_name_bytes().map(|b| b.to_vec()).unwrap_or_default();
                let nid = self.nodes.len();
                self.nodes.push(VNode { mode: S_IFLNK | 0o777, uid, gid, mtime, kind: NodeKind::Symlink { target } });
                if let NodeKind::Dir { children } = &mut self.nodes[parent_id].kind {
                    children.insert(leaf.clone(), nid);
                }
            } else if kind.is_fifo() {
                let nid = self.nodes.len();
                self.nodes.push(VNode { mode: S_IFIFO | perm, uid, gid, mtime, kind: NodeKind::Special });
                if let NodeKind::Dir { children } = &mut self.nodes[parent_id].kind {
                    children.insert(leaf.clone(), nid);
                }
            } else if kind.is_file() || kind == tar::EntryType::Continuous {
                let start = self.stage_pos;
                let written = io::copy(&mut entry, &mut self.stage)?;
                self.stage_pos += written;
                let nid = self.nodes.len();
                self.nodes.push(VNode {
                    mode: S_IFREG | perm,
                    uid,
                    gid,
                    mtime,
                    kind: NodeKind::File { stage_off: start, size: written },
                });
                if let NodeKind::Dir { children } = &mut self.nodes[parent_id].kind {
                    children.insert(leaf.clone(), nid);
                }
            } else if entry.header().entry_type().as_byte() == b's' {
                let nid = self.nodes.len();
                self.nodes.push(VNode { mode: S_IFSOCK | perm, uid, gid, mtime, kind: NodeKind::Special });
                if let NodeKind::Dir { children } = &mut self.nodes[parent_id].kind {
                    children.insert(leaf.clone(), nid);
                }
            }
        }
        Ok(())
    }
}

fn clean_components(path: &Path) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => out.push(p.as_bytes().to_vec()),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir | Component::Prefix(_) => return Vec::new(),
        }
    }
    out
}

fn build_erofs_from_layers(blobs: &BlobStore, layers: &[Layer], stage_path: &Path, out_path: &Path) -> Result<()> {
    let mut vfs = Vfs::new(stage_path)?;
    for layer in layers {
        let hex = layer
            .digest
            .strip_prefix("sha256:")
            .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .with_context(|| format!("bad layer digest `{}`", layer.digest))?;
        let blob_path = blobs.root().join("sha256").join(hex);
        unpack_blob_into(&blob_path, &mut vfs).with_context(|| format!("reading layer {}", &layer.digest[..19.min(layer.digest.len())]))?;
    }
    vfs.stage.flush()?;
    drop(vfs.stage);
    emit_erofs(&vfs.nodes, stage_path, out_path)
}

fn unpack_blob_into(blob_path: &Path, vfs: &mut Vfs) -> Result<()> {
    let mut f = File::open(blob_path).with_context(|| format!("opening {}", blob_path.display()))?;
    let mut magic = [0u8; 4];
    let n = f.read(&mut magic)?;
    f.seek(SeekFrom::Start(0))?;
    if n >= 2 && magic[..2] == [0x1f, 0x8b] {
        vfs.apply_tar_stream(GzDecoder::new(BufReader::with_capacity(1 << 20, f)))
    } else if n == 4 && is_zstd_magic(magic) {
        vfs.apply_tar_stream(ZstdFramesReader::new(BufReader::with_capacity(1 << 20, f)))
    } else {
        vfs.apply_tar_stream(BufReader::with_capacity(1 << 20, f))
    }
}

fn is_zstd_magic(m: [u8; 4]) -> bool {
    let word = u32::from_le_bytes(m);
    word == 0xFD2F_B528 || (0x184D_2A50..=0x184D_2A5F).contains(&word)
}

struct ZstdFramesReader<R: BufRead> {
    source: R,
    decoder: FrameDecoder,
    in_frame: bool,
    eof: bool,
}

impl<R: BufRead> ZstdFramesReader<R> {
    fn new(source: R) -> Self {
        Self { source, decoder: FrameDecoder::new(), in_frame: false, eof: false }
    }

    fn advance_frame(&mut self) -> io::Result<bool> {
        loop {
            if self.source.fill_buf()?.is_empty() {
                self.eof = true;
                self.in_frame = false;
                return Ok(false);
            }
            match self.decoder.reset(&mut self.source) {
                Ok(()) => {
                    self.in_frame = true;
                    return Ok(true);
                }
                Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame { length, .. })) => {
                    let skipped = io::copy(&mut (&mut self.source).take(u64::from(length)), &mut io::sink())?;
                    if skipped < u64::from(length) {
                        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated zstd skippable frame"));
                    }
                }
                Err(err) => return Err(io::Error::other(format!("zstd header: {err}"))),
            }
        }
    }
}

impl<R: BufRead> Read for ZstdFramesReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.eof {
            return Ok(0);
        }
        loop {
            if !self.in_frame && !self.advance_frame()? {
                return Ok(0);
            }
            while self.decoder.can_collect() == 0 && !self.decoder.is_finished() {
                self.decoder
                    .decode_blocks(&mut self.source, BlockDecodingStrategy::UptoBlocks(1))
                    .map_err(|err| io::Error::other(format!("zstd decode: {err}")))?;
            }
            if self.decoder.can_collect() > 0 {
                return self.decoder.read(buf);
            }
            self.in_frame = false;
        }
    }
}

struct DirData {
    blocks: Vec<[u8; BLK_SIZE]>,
    size: u64,
}

fn file_type_of(node: &VNode) -> u8 {
    match &node.kind {
        NodeKind::Dir { .. } => FT_DIR,
        NodeKind::File { .. } => FT_REG_FILE,
        NodeKind::Symlink { .. } => FT_SYMLINK,
        NodeKind::Special => match node.mode & S_IFMT {
            S_IFIFO => FT_FIFO,
            S_IFSOCK => FT_SOCK,
            _ => FT_REG_FILE,
        },
    }
}

fn encode_dir_blocks(entries: Vec<(Vec<u8>, u64, u8)>) -> Result<DirData> {
    let mut blocks: Vec<[u8; BLK_SIZE]> = Vec::new();
    let mut i = 0usize;
    let mut last_used = 0usize;

    while i < entries.len() {
        let mut count = 0usize;
        let mut names_len = 0usize;
        while i + count < entries.len() {
            let name_len = entries[i + count].0.len();
            if name_len == 0 || name_len > 255 {
                bail!("invalid EROFS entry name length {name_len}");
            }
            let next_need = (count + 1) * 12 + names_len + name_len;
            if next_need > BLK_SIZE {
                break;
            }
            count += 1;
            names_len += name_len;
        }
        if count == 0 {
            bail!("EROFS entry exceeded block size");
        }
        let mut blk = [0u8; BLK_SIZE];
        let mut name_off = count * 12;
        for (j, (name, nid, ftype)) in entries[i..i + count].iter().enumerate() {
            let d = &mut blk[j * 12..(j + 1) * 12];
            d[0..8].copy_from_slice(&nid.to_le_bytes());
            d[8..10].copy_from_slice(&(name_off as u16).to_le_bytes());
            d[10] = *ftype;
            d[11] = 0;
            blk[name_off..name_off + name.len()].copy_from_slice(name);
            name_off += name.len();
        }
        last_used = name_off;
        blocks.push(blk);
        i += count;
    }

    let size = ((blocks.len() - 1) * BLK_SIZE + last_used) as u64;
    Ok(DirData { blocks, size })
}

fn emit_erofs(nodes: &[VNode], stage_path: &Path, out_path: &Path) -> Result<()> {
    // 1. Walk reachable tree from root (0) to assign compact EROFS inode indices and exact nlinks.
    let mut old_to_new: HashMap<usize, usize> = HashMap::new();
    let mut reachable: Vec<usize> = Vec::new();
    let mut parent_of_dir: HashMap<usize, usize> = HashMap::new();
    let mut nlinks: Vec<u32> = Vec::new();

    old_to_new.insert(0, 0);
    reachable.push(0);
    parent_of_dir.insert(0, 0);
    nlinks.push(2); // "." and ".."

    let mut q_idx = 0usize;
    while q_idx < reachable.len() {
        let cur_old = reachable[q_idx];
        let cur_new = q_idx;
        q_idx += 1;
        if let NodeKind::Dir { children } = &nodes[cur_old].kind {
            for &child_old in children.values() {
                if let std::collections::hash_map::Entry::Vacant(e) = old_to_new.entry(child_old) {
                    let child_new = reachable.len();
                    e.insert(child_new);
                    reachable.push(child_old);
                    let init_nlink = if matches!(nodes[child_old].kind, NodeKind::Dir { .. }) {
                        parent_of_dir.insert(child_new, cur_new);
                        2
                    } else {
                        1
                    };
                    nlinks.push(init_nlink);
                } else if !matches!(nodes[child_old].kind, NodeKind::Dir { .. }) {
                    let child_new = old_to_new[&child_old];
                    nlinks[child_new] = nlinks[child_new].saturating_add(1);
                }
                if matches!(nodes[child_old].kind, NodeKind::Dir { .. }) {
                    nlinks[cur_new] = nlinks[cur_new].saturating_add(1);
                }
            }
        }
    }

    // 2. Build directory payloads in sorted byte order.
    let mut dir_data: HashMap<usize, DirData> = HashMap::new();
    for (new_idx, &old_idx) in reachable.iter().enumerate() {
        if let NodeKind::Dir { children } = &nodes[old_idx].kind {
            let parent_new = parent_of_dir[&new_idx];
            let mut entries: Vec<(Vec<u8>, u64, u8)> = Vec::with_capacity(children.len() + 2);
            entries.push((b".".to_vec(), (new_idx as u64) * 2, FT_DIR));
            entries.push((b"..".to_vec(), (parent_new as u64) * 2, FT_DIR));
            for (name, &child_old) in children {
                let child_new = old_to_new[&child_old];
                entries.push((name.clone(), (child_new as u64) * 2, file_type_of(&nodes[child_old])));
            }
            entries.sort_by(|a, b| a.0.as_slice().cmp(b.0.as_slice()));
            dir_data.insert(new_idx, encode_dir_blocks(entries)?);
        }
    }

    // 3. Assign metadata and data block addresses.
    let meta_blkaddr: u32 = 1;
    let inode_bytes = reachable.len() * 64;
    let meta_blocks = inode_bytes.div_ceil(BLK_SIZE);
    let mut next_blk = meta_blkaddr + meta_blocks as u32;

    struct ExtentPlan {
        size: u64,
        blkaddr: u32,
    }
    let mut plans: Vec<ExtentPlan> = Vec::with_capacity(reachable.len());
    for (new_idx, &old_idx) in reachable.iter().enumerate() {
        let (size, blks) = match &nodes[old_idx].kind {
            NodeKind::Dir { .. } => {
                let d = &dir_data[&new_idx];
                (d.size, d.blocks.len() as u32)
            }
            NodeKind::Symlink { target } => (target.len() as u64, target.len().div_ceil(BLK_SIZE) as u32),
            NodeKind::File { size, .. } => (*size, size.div_ceil(BLK_SIZE as u64) as u32),
            NodeKind::Special => (0, 0),
        };
        let blkaddr = if blks > 0 {
            let start = next_blk;
            next_blk = next_blk.checked_add(blks).context("EROFS block count overflow")?;
            start
        } else {
            0
        };
        plans.push(ExtentPlan { size, blkaddr });
    }
    let total_blocks = next_blk;

    // 4. Write EROFS image sequentially: superblock (blk 0), inode table (blk 1..), data blocks.
    let out_file = File::create(out_path).with_context(|| format!("creating {}", out_path.display()))?;
    let mut w = BufWriter::with_capacity(256 * 1024, out_file);

    let mut blk0 = [0u8; BLK_SIZE];
    {
        let sb = &mut blk0[EROFS_SUPER_OFFSET as usize..EROFS_SUPER_OFFSET as usize + 128];
        sb[0..4].copy_from_slice(&EROFS_SUPER_MAGIC_V1.to_le_bytes());
        sb[12] = EROFS_BLKSZBITS;
        sb[13] = 0; // sb_extslots
        sb[14..16].copy_from_slice(&0u16.to_le_bytes()); // root_nid = 0
        sb[16..24].copy_from_slice(&(reachable.len() as u64).to_le_bytes());
        sb[36..40].copy_from_slice(&total_blocks.to_le_bytes());
        sb[40..44].copy_from_slice(&meta_blkaddr.to_le_bytes());
    }
    w.write_all(&blk0)?;

    for (new_idx, &old_idx) in reachable.iter().enumerate() {
        let node = &nodes[old_idx];
        let plan = &plans[new_idx];
        let mut ino = [0u8; 64];
        ino[0..2].copy_from_slice(&EROFS_I_VERSION_EXTENDED.to_le_bytes());
        ino[4..6].copy_from_slice(&node.mode.to_le_bytes());
        ino[8..16].copy_from_slice(&plan.size.to_le_bytes());
        ino[16..20].copy_from_slice(&plan.blkaddr.to_le_bytes());
        ino[20..24].copy_from_slice(&((new_idx + 1) as u32).to_le_bytes());
        ino[24..28].copy_from_slice(&node.uid.to_le_bytes());
        ino[28..32].copy_from_slice(&node.gid.to_le_bytes());
        ino[32..40].copy_from_slice(&node.mtime.to_le_bytes());
        ino[44..48].copy_from_slice(&nlinks[new_idx].to_le_bytes());
        w.write_all(&ino)?;
    }
    let meta_rem = inode_bytes % BLK_SIZE;
    if meta_rem != 0 {
        let pad = vec![0u8; BLK_SIZE - meta_rem];
        w.write_all(&pad)?;
    }

    let mut stage_reader = BufReader::with_capacity(256 * 1024, File::open(stage_path)?);
    let mut copy_buf = vec![0u8; 64 * 1024];
    for (new_idx, &old_idx) in reachable.iter().enumerate() {
        match &nodes[old_idx].kind {
            NodeKind::Dir { .. } => {
                for blk in &dir_data[&new_idx].blocks {
                    w.write_all(blk)?;
                }
            }
            NodeKind::Symlink { target } if !target.is_empty() => {
                w.write_all(target)?;
                let rem = target.len() % BLK_SIZE;
                if rem != 0 {
                    w.write_all(&vec![0u8; BLK_SIZE - rem])?;
                }
            }
            NodeKind::File { stage_off, size } if *size > 0 => {
                stage_reader.seek(SeekFrom::Start(*stage_off))?;
                let mut left = *size;
                while left > 0 {
                    let want = (copy_buf.len() as u64).min(left) as usize;
                    stage_reader.read_exact(&mut copy_buf[..want])?;
                    w.write_all(&copy_buf[..want])?;
                    left -= want as u64;
                }
                let rem = (*size as usize) % BLK_SIZE;
                if rem != 0 {
                    w.write_all(&vec![0u8; BLK_SIZE - rem])?;
                }
            }
            _ => {}
        }
    }

    w.flush()?;
    let out_file = w.into_inner().map_err(|e| e.into_error())?;
    let actual_bytes = u64::from(total_blocks) * (BLK_SIZE as u64);
    if actual_bytes < EROFS_LOGICAL_BYTES {
        out_file.set_len(EROFS_LOGICAL_BYTES)?;
    }
    Ok(())
}

/// Remove any cached `.erofs` image files not referenced by `active_keys`.
pub fn prune_unreferenced_images(active_keys: &HashSet<String>, dry_run: bool) -> (u32, u64) {
    let dir = paths::images_dir();
    let Ok(entries) = fs::read_dir(&dir) else { return (0, 0) };
    let mut count = 0u32;
    let mut freed = 0u64;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Some(key) = name.strip_suffix(".erofs") else { continue };
        if active_keys.contains(key) {
            continue;
        }
        let bytes = allocated_bytes(&entry.path());
        if dry_run || fs::remove_file(entry.path()).is_ok() {
            count += 1;
            freed += bytes;
        }
    }
    (count, freed)
}

/// Allocated disk bytes under `<support_dir>/images/`.
pub fn images_allocated_bytes() -> u64 {
    let dir = paths::images_dir();
    let Ok(entries) = fs::read_dir(&dir) else { return 0 };
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".erofs"))
        .map(|e| allocated_bytes(&e.path()))
        .sum()
}

fn allocated_bytes(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).map_or(0, |m| m.blocks() * 512)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};

    use flate2::{Compression, write::GzEncoder};
    use graviton_image::BlobStore;

    use super::*;

    fn build_tar_bytes<F: FnOnce(&mut tar::Builder<Vec<u8>>)>(f: F) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        f(&mut b);
        b.into_inner().unwrap()
    }

    fn append_file(b: &mut tar::Builder<Vec<u8>>, path: &str, data: &[u8], mode: u32) {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(mode);
        h.set_cksum();
        b.append_data(&mut h, path, data).unwrap();
    }

    fn append_dir(b: &mut tar::Builder<Vec<u8>>, path: &str, mode: u32) {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_size(0);
        h.set_mode(mode);
        h.set_cksum();
        b.append_data(&mut h, path, io::empty()).unwrap();
    }

    fn gzip_bytes(raw: &[u8]) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(raw).unwrap();
        enc.finish().unwrap()
    }

    fn put_blob(blobs: &BlobStore, data: &[u8]) -> String {
        let digest = graviton_image::Digest::of(data);
        blobs.write_verified(&digest, data, |_| {}).unwrap();
        digest.to_string()
    }

    #[test]
    fn builds_valid_erofs_with_whiteouts_links_and_multi_block_dirs() {
        let dir = std::env::temp_dir().join(format!("graviton-erofs-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let blobs = BlobStore::new(dir.join("blobs"));

        // Layer 1: gzip tar with dir, file, hardlink, symlink, opaque-victim, whiteout-victim, and 350 files (> 1 4KiB block).
        let tar1 = build_tar_bytes(|b| {
            append_dir(b, "etc", 0o755);
            append_file(b, "etc/hello.txt", b"hello erofs", 0o644);
            let mut hl = tar::Header::new_gnu();
            hl.set_entry_type(tar::EntryType::Link);
            hl.set_size(0);
            hl.set_mode(0o644);
            hl.set_link_name("etc/hello.txt").unwrap();
            hl.set_cksum();
            b.append_data(&mut hl, "etc/hello-link.txt", io::empty()).unwrap();

            let mut sl = tar::Header::new_gnu();
            sl.set_entry_type(tar::EntryType::Symlink);
            sl.set_size(0);
            sl.set_mode(0o777);
            sl.set_link_name("hello.txt").unwrap();
            sl.set_cksum();
            b.append_data(&mut sl, "etc/hello.lnk", io::empty()).unwrap();

            append_file(b, "etc/deleted.txt", b"gone", 0o644);
            append_dir(b, "opdir", 0o755);
            append_file(b, "opdir/old.txt", b"old", 0o644);

            append_dir(b, "many", 0o755);
            for idx in 0..350 {
                append_file(b, &format!("many/item-{idx:03}.txt"), b"x", 0o644);
            }
        });
        let gz1 = gzip_bytes(&tar1);
        let l1 = put_blob(&blobs, &gz1);

        // Layer 2: raw tar deleting etc/deleted.txt and opaquing opdir/.
        let tar2 = build_tar_bytes(|b| {
            append_file(b, "etc/.wh.deleted.txt", b"", 0o644);
            append_file(b, "opdir/.wh..wh..opq", b"", 0o644);
            append_file(b, "opdir/new.txt", b"fresh", 0o644);
        });
        let l2 = put_blob(&blobs, &tar2);

        let layers = vec![
            Layer { digest: l1, size: gz1.len() as u64, media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into() },
            Layer { digest: l2, size: tar2.len() as u64, media_type: "application/vnd.oci.image.layer.v1.tar".into() },
        ];
        let stage = dir.join("stage.bin");
        let out = dir.join("image.erofs");
        build_erofs_from_layers(&blobs, &layers, &stage, &out).unwrap();

        let mut f = File::open(&out).unwrap();
        assert_eq!(f.metadata().unwrap().len(), EROFS_LOGICAL_BYTES);

        let mut sb = [0u8; 128];
        f.seek(SeekFrom::Start(EROFS_SUPER_OFFSET)).unwrap();
        f.read_exact(&mut sb).unwrap();
        assert_eq!(u32::from_le_bytes(sb[0..4].try_into().unwrap()), EROFS_SUPER_MAGIC_V1);
        assert_eq!(sb[12], EROFS_BLKSZBITS);
        assert_eq!(u16::from_le_bytes(sb[14..16].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(sb[40..44].try_into().unwrap()), 1);

        // Read root extended inode at blk 1 (offset 4096).
        let mut root_ino = [0u8; 64];
        f.seek(SeekFrom::Start(4096)).unwrap();
        f.read_exact(&mut root_ino).unwrap();
        assert_eq!(u16::from_le_bytes(root_ino[0..2].try_into().unwrap()), EROFS_I_VERSION_EXTENDED);
        assert_eq!(u16::from_le_bytes(root_ino[4..6].try_into().unwrap()), S_IFDIR | 0o755);
        // Root has ".", "..", "etc", "many", "opdir" -> 5 links.
        assert_eq!(u32::from_le_bytes(root_ino[44..48].try_into().unwrap()), 5);

        let root_blk = u32::from_le_bytes(root_ino[16..20].try_into().unwrap());
        let mut dir_blk = [0u8; BLK_SIZE];
        f.seek(SeekFrom::Start(u64::from(root_blk) * BLK_SIZE as u64)).unwrap();
        f.read_exact(&mut dir_blk).unwrap();
        let nameoff0 = u16::from_le_bytes(dir_blk[8..10].try_into().unwrap()) as usize;
        assert_eq!(nameoff0, 5 * 12);

        // Verify sorted names in root block: ".", "..", "etc", "many", "opdir".
        let mut names = Vec::new();
        for k in 0..5 {
            let off = u16::from_le_bytes(dir_blk[k * 12 + 8..k * 12 + 10].try_into().unwrap()) as usize;
            let next = if k + 1 < 5 {
                u16::from_le_bytes(dir_blk[(k + 1) * 12 + 8..(k + 1) * 12 + 10].try_into().unwrap()) as usize
            } else {
                u64::from_le_bytes(root_ino[8..16].try_into().unwrap()) as usize
            };
            names.push(std::str::from_utf8(&dir_blk[off..next]).unwrap().to_string());
        }
        assert_eq!(names, vec![".", "..", "etc", "many", "opdir"]);

        // Verify "many" spanned > 1 directory block.
        let many_nid = u64::from_le_bytes(dir_blk[3 * 12..3 * 12 + 8].try_into().unwrap());
        let mut many_ino = [0u8; 64];
        f.seek(SeekFrom::Start(4096 + (many_nid / 2) * 64)).unwrap();
        f.read_exact(&mut many_ino).unwrap();
        let many_size = u64::from_le_bytes(many_ino[8..16].try_into().unwrap());
        assert!(many_size > BLK_SIZE as u64);

        let _ = fs::remove_dir_all(&dir);
    }
}

