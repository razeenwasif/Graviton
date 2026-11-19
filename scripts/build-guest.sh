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
# mke2fs, to format each VM's scratch disk, with the libraries it links against.
E2FS_PKGS=${E2FS_PKGS:-"e2fsprogs-1.47.2-r2 e2fsprogs-libs-1.47.2-r2 libcom_err-1.47.2-r2 libblkid-2.41.6-r1 libuuid-2.41.6-r1 libeconf-0.6.3-r0 musl-1.2.5-r13"}
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
e2fs_dirs=""
for p in $E2FS_PKGS; do e2fs_dirs="$e2fs_dirs $(fetch "$p")"; done

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
mkdir -p "$root/bin" "$root/sbin" "$root/dev" "$root/proc" "$root/sys" "$root/tmp" "$root/run" "$root/etc" "$root/lib/modules" "$root/usr/lib"
cp "$agent" "$root/init"
cp "$busybox_dir/bin/busybox.static" "$root/bin/busybox"
ln -s busybox "$root/bin/sh"

# mke2fs (dynamically linked against musl) for the scratch disk, its config,
# and the shared libraries it needs, where musl's loader looks.
for d in $e2fs_dirs; do
  [ -f "$d/sbin/mke2fs" ] && cp "$d/sbin/mke2fs" "$root/sbin/mke2fs"
  [ -f "$d/etc/mke2fs.conf" ] && cp "$d/etc/mke2fs.conf" "$root/etc/mke2fs.conf"
  [ -d "$d/usr/lib" ] && find "$d/usr/lib" -maxdepth 1 -name 'lib*.so*' -exec cp -P {} "$root/usr/lib/" \;
  [ -d "$d/lib" ] && find "$d/lib" -maxdepth 1 -name '*musl*' -exec cp -P {} "$root/lib/" \;
done
[ -x "$root/sbin/mke2fs" ] && [ -e "$root/lib/ld-musl-aarch64.so.1" ] || { echo "mke2fs or musl missing from the guest" >&2; exit 1; }

moddir="$kernel_dir/lib/modules/$(ls "$kernel_dir/lib/modules")/kernel"
: > "$root/lib/modules/order"
# vsock for the agent channel; fuse + virtiofs for the image store and volumes;
# virtio-net (with its failover deps) and af_packet for networking and DHCP;
# virtio-blk and ext4 (with its deps, in order) for the scratch disk.
for m in net/vmw_vsock/vsock net/vmw_vsock/vmw_vsock_virtio_transport_common net/vmw_vsock/vmw_vsock_virtio_transport \
         fs/fuse/fuse fs/fuse/virtiofs \
         net/core/failover drivers/net/net_failover drivers/net/virtio_net net/packet/af_packet \
         lib/crc16 crypto/crc32c_generic fs/mbcache fs/jbd2/jbd2 fs/ext4/ext4 drivers/block/virtio_blk; do
  name=$(basename "$m")
  gunzip -c "$moddir/$m.ko.gz" > "$root/lib/modules/$name.ko"
  echo "$name.ko" >> "$root/lib/modules/order"
done

# udhcpc calls this with the lease; the agent reads /run/graviton-ip.
cat > "$root/etc/udhcpc.script" <<'SCRIPT'
#!/bin/sh
case "$1" in
  deconfig)
    ip addr flush dev "$interface"
    ip link set "$interface" up
    ;;
  bound|renew)
    ip addr flush dev "$interface"
    ip addr add "$ip/${mask:-24}" dev "$interface"
    [ -n "$router" ] && ip route replace default via "${router%% *}" dev "$interface"
    : > /etc/resolv.conf
    for server in $dns; do echo "nameserver $server" >> /etc/resolv.conf; done
    echo "$ip" > /run/graviton-ip
    ;;
esac
exit 0
SCRIPT
chmod 755 "$root/etc/udhcpc.script"

(cd "$root" && find . | sort | cpio -o -H newc --quiet) | gzip -9 > "$out/initrd.img"
printf 'Initramfs: %s KiB\n' "$(( $(stat -f %z "$out/initrd.img") / 1024 ))"
echo "Guest written to $out"
