#!/bin/sh
# Build Graviton's purpose-built arm64 Linux 6.12 kernel with all required
# virtio, vsock, virtiofs, ext4, overlayfs, balloon and binfmt_misc drivers
# compiled in (zero loadable modules).
#
# Runs the Linux kernel compilation inside a Graviton container itself (no
# external Linux machine or cross-compiler needed). The compiled arm64 Image is
# cached under ~/Library/Caches/Graviton/downloads/custom-kernel-<ver>-<sha>/
# so `scripts/build-guest.sh` picks it up automatically on subsequent builds.
#
# Usage:
#   scripts/build-kernel.sh            # build (or reuse cached) custom kernel
#   scripts/build-kernel.sh --force    # rebuild even if cached
set -eu

KERNEL_VERSION=${KERNEL_VERSION:-6.12.118}
KERNEL_URL="https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KERNEL_VERSION}.tar.xz"

force=0
for arg in "$@"; do
  case "$arg" in
    --force) force=1 ;;
    -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
    *) echo "build-kernel: unknown option $arg" >&2; exit 2 ;;
  esac
done

repo=$(cd "$(dirname "$0")/.." && pwd)
cfg="$repo/scripts/kernel.config"
cache="$HOME/Library/Caches/Graviton/downloads"
mkdir -p "$cache"

cfg_sha=$(shasum -a 256 "$cfg" | cut -c1-12)
out_dir="$cache/custom-kernel-${KERNEL_VERSION}-${cfg_sha}"
out_img="$out_dir/vmlinux"

if [ "$force" = 0 ] && [ -f "$out_img" ]; then
  printf 'Using cached custom kernel: %s (%s MiB)\n' "$out_img" "$(python3 -I -c "import os; print(f'{os.path.getsize(\"$out_img\") / 2**20:.1f}')")"
  exit 0
fi

tarball="$cache/linux-${KERNEL_VERSION}.tar.xz"
if [ ! -f "$tarball" ]; then
  echo "Downloading linux-${KERNEL_VERSION}.tar.xz..." >&2
  curl -fSL -o "$tarball.tmp" "$KERNEL_URL"
  mv "$tarball.tmp" "$tarball"
fi

grav=${GRAV:-}
if [ -z "$grav" ]; then
  if [ -x /Applications/Graviton.app/Contents/MacOS/grav ]; then
    grav=/Applications/Graviton.app/Contents/MacOS/grav
  elif command -v grav >/dev/null 2>&1; then
    grav=$(command -v grav)
  else
    echo "build-kernel: need a running local gravitond and grav to compile the Linux kernel" >&2
    exit 1
  fi
fi

"$grav" --host local ping >/dev/null 2>&1 || {
  echo "build-kernel: local gravitond isn't running (start it first, or run scripts/build-guest.sh --alpine)" >&2
  exit 1
}

work=$(mktemp -d /tmp/graviton-kbuild.XXXXXX)
trap 'rm -rf "$work"' EXIT
cp "$cfg" "$work/kernel.config"
mkdir -p "$work/out"

cpus=$(sysctl -n hw.ncpu 2>/dev/null || echo 8)
[ "$cpus" -gt 8 ] && cpus=8

echo "Building Linux ${KERNEL_VERSION}-graviton (arm64, ${cpus} vCPU) inside a Graviton container..." >&2
"$grav" --host local run --rm \
  --cpus "$cpus" --memory 4096 \
  -v "$cache:/downloads:ro" \
  -v "$work:/work" \
  alpine:3.22 sh -eu -c "
    apk add --no-cache build-base flex bison bc perl findutils xz linux-headers >/dev/null
    mkdir -p /tmp/src
    tar -xJf /downloads/linux-${KERNEL_VERSION}.tar.xz -C /tmp/src
    cd /tmp/src/linux-${KERNEL_VERSION}
    make ARCH=arm64 KCONFIG_ALLCONFIG=/work/kernel.config allnoconfig >/dev/null
    # Verify critical symbols stayed enabled after dependency resolution
    for sym in CONFIG_VIRTIO_VSOCKETS CONFIG_VIRTIO_FS CONFIG_VIRTIO_NET CONFIG_VIRTIO_BLK CONFIG_VIRTIO_BALLOON CONFIG_EXT4_FS CONFIG_OVERLAY_FS CONFIG_BINFMT_MISC CONFIG_VIRTIO_CONSOLE CONFIG_PCI_HOST_GENERIC; do
      grep -q \"^\${sym}=y\$\" .config || { echo \"MISSING REQUIRED SYMBOL: \$sym\" >&2; exit 1; }
    done
    make -j${cpus} ARCH=arm64 Image
    cp arch/arm64/boot/Image /work/out/vmlinux
    cp .config /work/out/config
  "

mkdir -p "$out_dir"
mv "$work/out/vmlinux" "$out_img.tmp"
mv "$work/out/config" "$out_dir/config"
mv "$out_img.tmp" "$out_img"

python3 -I - "$out_img" <<'PY'
import sys
p = sys.argv[1]
data = open(p, "rb").read()
if data[56:60] != b"ARM\x64":
    sys.exit(f"{p}: not an arm64 Linux Image")
print(f"Custom kernel ready: {p} ({len(data) / 2**20:.1f} MiB arm64 Image)")
PY
