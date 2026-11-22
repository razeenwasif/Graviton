//! Bounded on-disk persistence for per-container logs and resource stats.
//!
//! - Container logs (`vms/<id>/log.jsonl`) rotate atomically to `log.1.jsonl`
//!   when appending a line would exceed [`LOG_MAX_BYTES`], capping disk usage
//!   at ~20 MiB per container while keeping `read_logs_tail` streaming/bounded.
//! - Container resource samples (`vms/<id>/stats.jsonl`) append 1 line per
//!   sample and compact atomically back to `keep` samples whenever file size
//!   exceeds [`STATS_COMPACT_BYTES`].

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use graviton_proto::{LogLine, StatSample, decode_line, encode_line};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Rotate active `log.jsonl` -> `log.1.jsonl` once a write would exceed 10 MiB.
pub const LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Compact `stats.jsonl` back down to the kept tail once it exceeds 96 KiB.
pub const STATS_COMPACT_BYTES: u64 = 96 * 1024;

/// Path of the single rotated backup beside `path` (`log.jsonl` -> `log.1.jsonl`).
pub fn rotated_log_path(path: &Path) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("log");
    match path.extension().and_then(|s| s.to_str()) {
        Some(ext) if !ext.is_empty() => path.with_file_name(format!("{stem}.1.{ext}")),
        _ => path.with_file_name(format!("{stem}.1")),
    }
}

/// Append one log line to `path`, rotating to `log.1.jsonl` at [`LOG_MAX_BYTES`].
pub fn append_log(path: &Path, line: &LogLine) {
    append_log_capped(path, line, LOG_MAX_BYTES);
}

pub fn append_log_capped(path: &Path, line: &LogLine, max_bytes: u64) {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let encoded = encode_line(line);
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() > 0 && meta.len().saturating_add(encoded.len() as u64) > max_bytes {
            let _ = fs::rename(path, rotated_log_path(path));
        }
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(&encoded);
    }
}

/// Stream up to `limit` newest log lines across `[log.1.jsonl, log.jsonl]`.
/// Oversized legacy `log.jsonl` files exceeding [`LOG_MAX_BYTES`] are compacted
/// atomically on load.
pub fn read_logs_tail(path: &Path, limit: usize) -> Vec<LogLine> {
    read_logs_tail_capped(path, limit, LOG_MAX_BYTES)
}

pub fn read_logs_tail_capped(path: &Path, limit: usize, max_bytes: u64) -> Vec<LogLine> {
    if limit == 0 {
        return Vec::new();
    }
    let backup = rotated_log_path(path);
    let mut kept = VecDeque::with_capacity(limit.min(1024));
    read_tail_into(&backup, limit, &mut kept);
    read_tail_into(path, limit, &mut kept);
    if fs::metadata(path).is_ok_and(|m| m.len() > max_bytes) {
        let _ = fs::remove_file(&backup);
        rewrite_jsonl_atomic(path, &kept);
    }
    kept.into_iter().collect()
}

/// Append one resource sample to `path`, compacting to `keep` once over [`STATS_COMPACT_BYTES`].
pub fn append_stat_sample(path: &Path, sample: &StatSample, keep: usize) {
    append_stat_sample_capped(path, sample, keep, STATS_COMPACT_BYTES);
}

pub fn append_stat_sample_capped(path: &Path, sample: &StatSample, keep: usize, compact_bytes: u64) {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(&encode_line(sample));
    }
    if keep > 0 && fs::metadata(path).is_ok_and(|m| m.len() > compact_bytes) {
        let mut kept: VecDeque<StatSample> = VecDeque::with_capacity(keep);
        read_tail_into(path, keep, &mut kept);
        rewrite_jsonl_atomic(path, &kept);
    }
}

/// Reload up to `keep` newest stat samples from `path`, compacting oversized files.
pub fn read_stats_tail(path: &Path, keep: usize) -> VecDeque<StatSample> {
    read_stats_tail_capped(path, keep, STATS_COMPACT_BYTES)
}

pub fn read_stats_tail_capped(path: &Path, keep: usize, compact_bytes: u64) -> VecDeque<StatSample> {
    if keep == 0 {
        return VecDeque::new();
    }
    let mut kept = VecDeque::with_capacity(keep);
    read_tail_into(path, keep, &mut kept);
    if fs::metadata(path).is_ok_and(|m| m.len() > compact_bytes) {
        rewrite_jsonl_atomic(path, &kept);
    }
    kept
}

fn read_tail_into<T: DeserializeOwned>(path: &Path, limit: usize, out: &mut VecDeque<T>) {
    let Ok(f) = File::open(path) else { return };
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        if let Ok(item) = decode_line::<T>(line.as_bytes()) {
            out.push_back(item);
            while out.len() > limit {
                out.pop_front();
            }
        }
    }
}

fn rewrite_jsonl_atomic<'a, T: Serialize + 'a>(path: &Path, items: impl IntoIterator<Item = &'a T>) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("data.jsonl");
    let tmp = path.with_file_name(format!(".{name}.{seq}.tmp"));
    let mut out = Vec::new();
    for item in items {
        out.extend(encode_line(item));
    }
    if fs::write(&tmp, out).and_then(|_| fs::rename(&tmp, path)).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graviton_proto::{LogLevel, LogStream};

    fn make_line(seq: u64, msg: &str) -> LogLine {
        LogLine {
            seq,
            timestamp_ms: 1_700_000_000_000 + seq,
            level: LogLevel::Info,
            message: msg.to_string(),
            stream: LogStream::Stdout,
        }
    }

    fn make_sample(i: u64) -> StatSample {
        StatSample {
            timestamp_ms: 1_700_000_000_000 + i * 2_000,
            cpu_percent: (i % 100) as f64,
            memory_bytes: (10 + i) * 1024 * 1024,
            memory_limit_bytes: 512 * 1024 * 1024,
            net_rx_bytes: i * 1000,
            net_tx_bytes: i * 500,
        }
    }

    #[test]
    fn rotates_logs_at_size_cap_and_reads_tail_in_order() {
        let dir = std::env::temp_dir().join(format!("graviton-persist-logs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let log_file = dir.join("log.jsonl");
        let backup = rotated_log_path(&log_file);
        assert_eq!(backup, dir.join("log.1.jsonl"));

        let cap = 512u64;
        for seq in 1..=40 {
            append_log_capped(&log_file, &make_line(seq, &format!("line-{seq:02}-payload")), cap);
        }

        // Inject a malformed trailing line into the active file; it should be ignored safely.
        let mut f = OpenOptions::new().append(true).open(&log_file).unwrap();
        writeln!(f, "{{corrupt-json").unwrap();
        drop(f);

        assert!(backup.is_file(), "expected rotated backup file to exist");
        assert!(fs::metadata(&log_file).unwrap().len() <= cap + 32);
        assert!(fs::metadata(&backup).unwrap().len() <= cap);

        let tail = read_logs_tail_capped(&log_file, 6, cap * 4);
        let seqs: Vec<u64> = tail.iter().map(|l| l.seq).collect();
        assert_eq!(seqs, vec![35, 36, 37, 38, 39, 40]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compacts_oversized_legacy_log_on_restore() {
        let dir = std::env::temp_dir().join(format!("graviton-persist-legacy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let log_file = dir.join("log.jsonl");

        // Write 60 lines without rotation (~6 KB total).
        {
            let mut f = File::create(&log_file).unwrap();
            for seq in 1..=60 {
                f.write_all(&encode_line(&make_line(seq, "legacy-unbounded-entry"))).unwrap();
            }
        }
        let before = fs::metadata(&log_file).unwrap().len();
        assert!(before > 2000);

        let kept = read_logs_tail_capped(&log_file, 10, 2000);
        let seqs: Vec<u64> = kept.iter().map(|l| l.seq).collect();
        assert_eq!(seqs, (51..=60).collect::<Vec<_>>());

        let after = fs::metadata(&log_file).unwrap().len();
        assert!(after < before, "expected compacted file ({after}) < original ({before})");
        assert!(!rotated_log_path(&log_file).exists());

        // Reloading again stays idempotent.
        let reloaded = read_logs_tail_capped(&log_file, 10, 2000);
        assert_eq!(reloaded, kept);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn appends_compacts_and_reloads_stats_samples() {
        let dir = std::env::temp_dir().join(format!("graviton-persist-stats-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let stats_file = dir.join("stats.jsonl");

        let keep = 20usize;
        let compact_bytes = 4 * 1024u64;
        for i in 1..=220u64 {
            append_stat_sample_capped(&stats_file, &make_sample(i), keep, compact_bytes);
        }

        let size_after_append = fs::metadata(&stats_file).unwrap().len();
        assert!(size_after_append <= compact_bytes + 512, "unexpected stats size {size_after_append}");

        // Inject corrupt trailing line to ensure tolerance across unexpected power loss.
        {
            let mut f = OpenOptions::new().append(true).open(&stats_file).unwrap();
            writeln!(f, "{{\"truncated\":").unwrap();
        }

        let reloaded = read_stats_tail_capped(&stats_file, keep, compact_bytes);
        assert_eq!(reloaded.len(), keep);
        let ts: Vec<u64> = reloaded.iter().map(|s| (s.timestamp_ms - 1_700_000_000_000) / 2_000).collect();
        assert_eq!(ts, (201..=220).collect::<Vec<_>>());

        let _ = fs::remove_dir_all(&dir);
    }
}
