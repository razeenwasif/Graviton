#!/bin/sh
# Builds the Graviton guest: an arm64 Linux kernel and an initramfs whose /init
# is graviton-agent. Output goes to ~/Library/Application Support/Graviton/guest
# (override with GRAVITON_GUEST_DIR), where gravitond looks by default.
#
# The kernel and busybox come from Alpine's signed packages, downloaded over
# HTTPS into ~/Library/Caches/Graviton/downloads and reused afterwards.
set -eu

ALPINE=${ALPINE:-v3.22}
KERNEL_PKG=${KERNEL_PKG:-linux-virt-6.12.115-r0}
BUSYBOX_PKG=${BUSYBOX_PKG:-busybox-static-1.37.0-r20}
MIRROR="https://dl-cdn.alpinelinux.org/alpine/$ALPINE/main/aarch64"

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

kernel_dir=$(fetch "$KERNEL_PKG")
busybox_dir=$(fetch "$BUSYBOX_PKG")

# 1. Kernel. Alpine ships an EFI zboot image; Virtualization.framework's Linux
#    boot loader wants the raw arm64 Image, so unwrap the gzip payload.
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
print(f"Kernel: {len(image) / 2**20:.1f} MiB arm64 Image")
PY

# 2. Agent, statically linked for the guest.
cargo build --quiet --manifest-path "$repo/Cargo.toml" -p graviton-agent --release --target aarch64-unknown-linux-musl
target_dir=$(cargo metadata --format-version 1 --no-deps --manifest-path "$repo/Cargo.toml" |
  python3 -I -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
agent="$target_dir/aarch64-unknown-linux-musl/release/graviton-agent"

# 3. Initramfs tree.
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
mkdir -p "$root/bin" "$root/dev" "$root/proc" "$root/sys" "$root/tmp" "$root/run" "$root/lib/modules"
cp "$agent" "$root/init"
cp "$busybox_dir/bin/busybox.static" "$root/bin/busybox"
ln -s busybox "$root/bin/sh"

modules="$kernel_dir/lib/modules/$(ls "$kernel_dir/lib/modules")/kernel/net/vmw_vsock"
: > "$root/lib/modules/order"
for m in vsock vmw_vsock_virtio_transport_common vmw_vsock_virtio_transport; do
  gunzip -c "$modules/$m.ko.gz" > "$root/lib/modules/$m.ko"
  echo "$m.ko" >> "$root/lib/modules/order"
done

(cd "$root" && find . | sort | cpio -o -H newc --quiet) | gzip -9 > "$out/initrd.img"
printf 'Initramfs: %s KiB\n' "$(( $(stat -f %z "$out/initrd.img") / 1024 ))"
echo "Guest written to $out"
