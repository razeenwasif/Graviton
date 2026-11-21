#!/bin/bash
# Update the engine of Graviton.app on another Mac (the mini, say) from this
# checkout: gravitond, grav and, with --guest, the guest kernel and initramfs.
# The app's own UI isn't touched, so this works on a Mac you only use as a
# server, and it doesn't need the Mac app to build.
#
#   scripts/deploy-remote.sh [host]            default host: mini
#   scripts/deploy-remote.sh local --guest     this Mac, without ssh
#   scripts/deploy-remote.sh mini --guest      also rebuild and install the guest
#   scripts/deploy-remote.sh mini --force      even while containers are running
#   scripts/deploy-remote.sh mini --app /Applications/Graviton.app
#
# What it gets right, each the lesson of a deploy that went wrong:
#
#   * Signing. gravitond reads the APNs key from that Mac's login Keychain,
#     which trusts the signature that stored it. An ad hoc signature changes
#     with every build, so the Keychain asks on that Mac's screen whether to
#     allow the new one (once, an unanswered dialog froze gravitond). Signing
#     with a certificate keeps the identity the same from build to build:
#     GRAVITON_SIGN_IDENTITY, else the first "Apple Development" identity in
#     this Mac's Keychain. Click Always Allow once; later deploys keep it.
#   * Replacing. macOS caches a binary's signature by file; overwriting a
#     signed binary in place gets the new one killed at launch
#     (OS_REASON_CODESIGNING). Each file goes in as a new file, with mv.
#   * Running containers. Restarting gravitond stops every VM, CI jobs
#     included, so it refuses while any container runs unless --force.
#   * Rolling back. If the new gravitond doesn't answer, the old files go back.
set -euo pipefail

host=mini
app=/Applications/Graviton.app
guest=0
force=0
while [ $# -gt 0 ]; do
    case "$1" in
        --guest) guest=1; shift ;;
        --force) force=1; shift ;;
        --app) app=${2:?--app needs the path to Graviton.app on that Mac}; shift 2 ;;
        -h|--help) sed -n '2,31p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        -*) echo "unknown option $1" >&2; exit 2 ;;
        *) host="$1"; shift ;;
    esac
done

repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
ssh_opts=(-o BatchMode=yes -o ConnectTimeout=10)

# What's being deployed: the commit, and whether the engine's sources differ from it.
echo "Deploying $(git rev-parse --short HEAD) ($(git log -1 --format=%s | cut -c1-60)) to $host"
if [ -n "$(git status --porcelain -- crates scripts Cargo.toml Cargo.lock)" ]; then
    echo "Note: crates/ or scripts/ have uncommitted changes, and they go too." >&2
fi

identity="${GRAVITON_SIGN_IDENTITY:-$(security find-identity -v -p codesigning | sed -n 's/.*"\(Apple Development[^"]*\)".*/\1/p' | head -1)}"
if [ -z "$identity" ]; then
    echo "No Apple Development certificate found; set GRAVITON_SIGN_IDENTITY." >&2
    exit 1
fi

# 1. Build and sign here.
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cargo build -q --release -p gravitond -p grav
target="$(cargo metadata --format-version 1 --no-deps | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/release"
cp "$target/gravitond" "$target/grav" "$stage/"
codesign --force --sign "$identity" --options runtime --timestamp=none --identifier dev.graviton.gravitond \
    --entitlements crates/gravitond/gravitond.entitlements "$stage/gravitond"
codesign --force --sign "$identity" --options runtime --timestamp=none --identifier dev.graviton.grav "$stage/grav"
files="gravitond grav"
if [ $guest = 1 ]; then
    mkdir -p "$stage/guest"
    GRAVITON_GUEST_DIR="$stage/guest" scripts/build-guest.sh
    files="$files guest/vmlinux guest/initrd.img"
fi

# 2. Copy to a staging folder there (or here, for `local`).
remote_stage=".graviton-deploy"
if [ "$host" = local ]; then
    rm -rf "$HOME/$remote_stage" && mkdir -p "$HOME/$remote_stage/guest"
    for f in $files; do cp "$stage/$f" "$HOME/$remote_stage/$f"; done
    there() { bash -s -- "$@"; }
else
    # shellcheck disable=SC2029
    ssh "${ssh_opts[@]}" "$host" "rm -rf $remote_stage && mkdir -p $remote_stage/guest"
    for f in $files; do
        scp -q "${ssh_opts[@]}" "$stage/$f" "$host:$remote_stage/$f"
    done
    # ssh runs one command line through the remote shell: quote the arguments,
    # or the list of files splits into words and only the first is deployed.
    there() { ssh "${ssh_opts[@]}" "$host" "bash -s -- $(printf '%q ' "$@")"; }
fi

# 3. Swap them in there, restart, check, and roll back if it doesn't answer.
# shellcheck disable=SC2087
there "$app" "$force" "$files" <<'REMOTE'
set -euo pipefail
app=$1 force=$2 files=$3
stage="$HOME/.graviton-deploy"
label=dev.graviton.gravitond
plist="$HOME/Library/LaunchAgents/$label.plist"
uid=$(id -u)
grav="$app/Contents/MacOS/grav"
check_grav="$grav"
[ -x "$check_grav" ] || check_grav="$stage/grav"
[ -d "$app" ] || { echo "No $app on $(hostname -s)." >&2; exit 1; }
[ -f "$plist" ] || { echo "No $plist: install gravitond as a launch agent there first." >&2; exit 1; }

dest() { # staged name -> path in the app
    case "$1" in
        guest/*) echo "$app/Contents/Resources/$1" ;;
        *) echo "$app/Contents/MacOS/$1" ;;
    esac
}

# `grav ps` lists running containers under a header line.
running=$("$check_grav" --host local ps 2>/dev/null | tail -n +2 | grep -c . || true)
if [ "$running" != "0" ] && [ "$force" != "1" ]; then
    echo "$running container(s) running on $(hostname -s); restarting gravitond would stop them. Wait, or pass --force." >&2
    exit 1
fi

# Keep the current files, then put each new one in as a new file.
mkdir -p "$stage/backup/guest"
for f in $files; do
    d=$(dest "$f")
    [ -e "$d" ] && cp -p "$d" "$stage/backup/$f"
done
launchctl bootout "gui/$uid/$label" 2>/dev/null || true
for _ in 1 2 3 4 5; do launchctl print "gui/$uid/$label" >/dev/null 2>&1 || break; sleep 1; done

install_files() { # from-dir
    for f in $files; do
        [ -e "$1/$f" ] || continue
        d=$(dest "$f")
        mkdir -p "$(dirname "$d")"
        cp -p "$1/$f" "$(dirname "$d")/.$(basename "$d").new"
        mv -f "$(dirname "$d")/.$(basename "$d").new" "$d"
    done
    # Reseal the bundle the way it was sealed: an ad hoc app stays ad hoc;
    # the binaries keep their own signatures.
    if codesign -dv "$app" 2>&1 | grep -q "Signature=adhoc"; then
        codesign --force --sign - --preserve-metadata=identifier,entitlements,flags "$app" 2>/dev/null
    fi
}

start_and_check() {
    launchctl bootstrap "gui/$uid" "$plist"
    for _ in $(seq 1 15); do
        "$grav" --host local ping >/dev/null 2>&1 && return 0
        sleep 1
    done
    return 1
}

install_files "$stage"
if start_and_check; then
    echo "gravitond is up on $(hostname -s): $("$grav" --host local ping)"
    codesign -dvv "$app/Contents/MacOS/gravitond" 2>&1 | sed -n 's/^Authority=\(Apple Development\).*/signed with your \1 certificate/p; s/^Signature=adhoc/signed ad hoc/p' | head -1
    rm -rf "$stage"
else
    reason=$(launchctl print "gui/$uid/$label" 2>/dev/null | sed -n 's/.*last exit reason = //p' | head -1)
    echo "The new gravitond didn't answer${reason:+ ($reason)}; putting the old files back." >&2
    launchctl bootout "gui/$uid/$label" 2>/dev/null || true
    sleep 2
    install_files "$stage/backup"
    if start_and_check; then echo "The old gravitond is back up." >&2; else echo "The old gravitond didn't come back either: look at ~/Library/Logs/Graviton/gravitond.log." >&2; fi
    exit 1
fi
REMOTE
