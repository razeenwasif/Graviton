#!/bin/sh
# Builds the Graviton guest: an arm64 Linux kernel and an initramfs whose /init
# is graviton-agent. Output goes to ~/Library/Application Support/Graviton/guest
# (override with GRAVITON_GUEST_DIR), where gravitond looks by default.
#
# Kernel selection:
#   - By default (auto), uses $GRAVITON_KERNEL if set, else the cached custom
#     Linux 6.12-graviton kernel built by `scripts/build-kernel.sh` (zero
#     loadable modules, ~8 MiB), else falls back to Alpine's signed linux-virt.
#   - `--custom` builds (or reuses) the custom kernel via `scripts/build-kernel.sh`.
#   - `--alpine` forces Alpine's linux-virt kernel and bundles its `.ko` modules.
set -eu

ALPINE=${ALPINE:-v3.22}
KERNEL_VERSION=${KERNEL_VERSION:-6.12.118}
KERNEL_PKG=${KERNEL_PKG:-linux-virt-6.12.118-r0}
MIRROR="https://dl-cdn.alpinelinux.org/alpine/$ALPINE/main/aarch64"

kernel_mode="auto"
for arg in "$@"; do
  case "$arg" in
    --custom) kernel_mode="custom" ;;
    --alpine) kernel_mode="alpine" ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "build-guest: unknown option $arg" >&2; exit 2 ;;
  esac
done

repo=$(cd "$(dirname "$0")/.." && pwd)
cache="$HOME/Library/Caches/Graviton/downloads"
out=${GRAVITON_GUEST_DIR:-"$HOME/Library/Application Support/Graviton/guest"}
mkdir -p "$cache" "$out"

fetch() { # package -> extracted directory
  pkg=$1
  dir="$cache/$pkg"
  if [ ! -d "$dir" ]; then
    echo "Downloading $pkg.apk" >&2
    curl -fsSL -o "$cache/$pkg.apk" "$MIRROR/$pkg.apk"
    mkdir -p "$dir.tmp"
    tar -xzf "$cache/$pkg.apk" -C "$dir.tmp" 2>/dev/null
    mv "$dir.tmp" "$dir"
  fi
  printf '%s' "$dir"
}

custom_kernel=""
if [ "$kernel_mode" != "alpine" ]; then
  if [ -n "${GRAVITON_KERNEL:-}" ]; then
    custom_kernel="$GRAVITON_KERNEL"
  else
    cfg_sha=$(shasum -a 256 "$repo/scripts/kernel.config" | cut -c1-12)
    cached_custom="$cache/custom-kernel-${KERNEL_VERSION}-${cfg_sha}/vmlinux"
    if [ "$kernel_mode" = "custom" ] && [ ! -f "$cached_custom" ]; then
      "$repo/scripts/build-kernel.sh"
    fi
    if [ -f "$cached_custom" ]; then
      custom_kernel="$cached_custom"
    fi
  fi
fi

# 1. Kernel.
kernel_dir=""
if [ -n "$custom_kernel" ]; then
  [ -f "$custom_kernel" ] || { echo "build-guest: custom kernel not found at $custom_kernel" >&2; exit 1; }
  python3 -I - "$custom_kernel" "$out/vmlinux" <<'PY'
import shutil, sys
src, dst = sys.argv[1], sys.argv[2]
data = open(src, "rb").read(64)
if data[56:60] != b"ARM\x64":
    sys.exit(f"{src}: not an arm64 Linux Image")
shutil.copyfile(src, dst)
size = open(dst, "rb").seek(0, 2)
print(f"Kernel: {size / 2**20:.1f} MiB arm64 Image (custom, 0 modules)")
PY
else
  kernel_dir=$(fetch "$KERNEL_PKG")
  # Alpine ships an EFI zboot image; Virtualization.framework's Linux
  # boot loader wants the raw arm64 Image, so unwrap the gzip payload.
  python3 -I - "$kernel_dir/boot/vmlinuz-virt" "$out/vmlinux" <<'PY'
import gzip, struct, sys
src, dst = sys.argv[1], sys.argv[2]
data = open(src, "rb").read()
if data[4:8] != b"zimg":
    sys.exit(f"{src}: not an EFI zboot image")
offset, size = struct.unpack_from("<II", data, 8)
kind = data[24:32].rstrip(b"\0").decode()
if kind != "gzip":
    sys.exit(f"{src}: unsupported zboot compression {kind!r}")
image = gzip.decompress(data[offset:offset + size])
if image[56:60] != b"ARM\x64":
    sys.exit("decompressed payload is not an arm64 Image")
open(dst, "wb").write(image)
print(f"Kernel: {len(image) / 2**20:.1f} MiB arm64 Image (Alpine linux-virt)")
PY
fi

# 2. Agent, statically linked and size-optimized for the guest.
cargo build --quiet --manifest-path "$repo/Cargo.toml" -p graviton-agent --profile guest --target aarch64-unknown-linux-musl
target_dir=$(cargo metadata --format-version 1 --no-deps --manifest-path "$repo/Cargo.toml" |
  python3 -I -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
agent="$target_dir/aarch64-unknown-linux-musl/guest/graviton-agent"

# 3. Minimal initramfs tree (agent /init only; kernel drivers are built-in unless --alpine).
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
mkdir -p "$root/dev" "$root/proc" "$root/sys" "$root/tmp" "$root/run" "$root/etc" "$root/lib/modules"
cp "$agent" "$root/init"

: > "$root/lib/modules/order"
if [ -n "$kernel_dir" ]; then
  moddir="$kernel_dir/lib/modules/$(ls "$kernel_dir/lib/modules")/kernel"
  # vsock for the agent channel; fuse + virtiofs for the image store, volumes and Rosetta;
  # virtio-net (with its failover deps) and af_packet for networking and DHCP;
  # virtio-blk, ext4, erofs and overlay for container roots;
  # binfmt_misc for Rosetta x86_64 execution and virtio_balloon for memory reclaim.
  for m in net/vmw_vsock/vsock net/vmw_vsock/vmw_vsock_virtio_transport_common net/vmw_vsock/vmw_vsock_virtio_transport \
           fs/fuse/fuse fs/fuse/virtiofs \
           net/core/failover drivers/net/net_failover drivers/net/virtio_net net/packet/af_packet \
           lib/crc16 crypto/crc32c_generic fs/mbcache fs/jbd2/jbd2 fs/ext4/ext4 fs/erofs/erofs fs/overlayfs/overlay drivers/block/virtio_blk \
           fs/binfmt_misc drivers/virtio/virtio_balloon; do
    if [ -f "$moddir/$m.ko.gz" ]; then
      name=$(basename "$m")
      gunzip -c "$moddir/$m.ko.gz" > "$root/lib/modules/$name.ko"
      echo "$name.ko" >> "$root/lib/modules/order"
    fi
  done
fi

(cd "$root" && find . | sort | cpio -o -H newc --quiet) | gzip -9 > "$out/initrd.img"
printf 'Initramfs: %s KiB\n' "$(( $(stat -f %z "$out/initrd.img") / 1024 ))"

# 4. Pre-formatted sparse 64 GiB ext4 template (GVSP v1) so guests never need mke2fs.
tpl="$cache/empty-disk-64g-ext4-j4.gvsp.gz"
if [ ! -f "$tpl" ]; then
  disk_tmp="$root/empty-disk.img"
  python3 -I - "$disk_tmp" <<'PY'
import sys
with open(sys.argv[1], "wb") as f:
    f.truncate(64 * 1024 * 1024 * 1024)
PY
  mke2fs_bin=""
  for cand in "$(command -v mke2fs 2>/dev/null || true)" \
              "/opt/homebrew/opt/e2fsprogs/sbin/mke2fs" \
              "/usr/local/opt/e2fsprogs/sbin/mke2fs" \
              "$HOME/Library/Android/sdk/platform-tools/mke2fs"; do
    if [ -n "$cand" ] && [ -x "$cand" ]; then
      if "$cand" -q -F -t ext4 -J size=4 -O ^metadata_csum \
          -E lazy_itable_init=1,nodiscard,hash_seed=67726176-6974-6f6e-0000-000000000001 \
          -U 67726176-6974-6f6e-0000-000000000002 -m 0 "$disk_tmp" >/dev/null 2>&1; then
        mke2fs_bin="$cand"
        break
      fi
      python3 -I - "$disk_tmp" <<'PY'
import sys
with open(sys.argv[1], "wb") as f:
    f.truncate(64 * 1024 * 1024 * 1024)
PY
    fi
  done
  if [ -z "$mke2fs_bin" ]; then
    grav_bin=${GRAV:-}
    [ -z "$grav_bin" ] && [ -x /Applications/Graviton.app/Contents/MacOS/grav ] && grav_bin=/Applications/Graviton.app/Contents/MacOS/grav
    [ -z "$grav_bin" ] && grav_bin=$(command -v grav 2>/dev/null || true)
    [ -n "$grav_bin" ] || { echo "build-guest: need mke2fs or grav to generate empty-disk.gvsp.gz" >&2; exit 1; }
    "$grav_bin" --host local run --rm -v "$root:/work" alpine:3.22 sh -eu -c \
      "apk add --no-cache e2fsprogs >/dev/null && mke2fs -q -F -t ext4 -J size=4 -O ^metadata_csum -E lazy_itable_init=1,nodiscard,hash_seed=67726176-6974-6f6e-0000-000000000001 -U 67726176-6974-6f6e-0000-000000000002 -m 0 /work/empty-disk.img"
  fi
  python3 -I - "$disk_tmp" "$tpl.tmp" <<'PY'
import gzip, os, struct, sys
src, dst = sys.argv[1], sys.argv[2]
total = os.path.getsize(src)
with open(src, "rb") as f, gzip.open(dst, "wb", compresslevel=9) as gz:
    gz.write(b"GVSP\x01\x00\x00\x00")
    gz.write(struct.pack("<Q", total))
    fd = f.fileno()
    off = 0
    used_seek = hasattr(os, "SEEK_DATA") and hasattr(os, "SEEK_HOLE")
    if used_seek:
        try:
            while off < total:
                data_start = os.lseek(fd, off, os.SEEK_DATA)
                hole_start = os.lseek(fd, data_start, os.SEEK_HOLE)
                pos = data_start
                while pos < hole_start:
                    f.seek(pos)
                    chunk = f.read(min(1 << 20, hole_start - pos))
                    if not chunk:
                        break
                    if any(chunk):
                        gz.write(struct.pack("<QI", pos, len(chunk)))
                        gz.write(chunk)
                    pos += len(chunk)
                off = hole_start
        except OSError as e:
            if e.errno != 6:  # ENXIO == no more data
                used_seek = False
    if not used_seek:
        f.seek(0)
        pos = 0
        while pos < total:
            chunk = f.read(4096)
            if not chunk:
                break
            if any(chunk):
                gz.write(struct.pack("<QI", pos, len(chunk)))
                gz.write(chunk)
            pos += len(chunk)
    gz.write(struct.pack("<QI", 0, 0))
PY
  mv "$tpl.tmp" "$tpl"
fi
cp "$tpl" "$out/empty-disk.gvsp.gz"
printf 'Template:  %s KiB (sparse 64 GiB ext4)\n' "$(( $(stat -f %z "$out/empty-disk.gvsp.gz") / 1024 ))"
echo "Guest written to $out"
