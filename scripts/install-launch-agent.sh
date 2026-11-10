#!/bin/sh
# Install gravitond as a launch agent, so it starts at login and restarts if it
# crashes. The signed binary goes to ~/Library/Application Support/Graviton/bin
# and logs to ~/Library/Logs/Graviton/gravitond.log.
#   scripts/install-launch-agent.sh            # install (or update) and start
#   scripts/install-launch-agent.sh --remote   # ...and accept paired phones (see grav pair)
#   scripts/install-launch-agent.sh --app /Applications/Graviton.app
#                                              # run the signed gravitond inside a release build
#   scripts/install-launch-agent.sh --uninstall
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
label=dev.graviton.gravitond
plist="$HOME/Library/LaunchAgents/$label.plist"
bin_dir="$HOME/Library/Application Support/Graviton/bin"
log_dir="$HOME/Library/Logs/Graviton"
domain="gui/$(id -u)"

remote_arg=""
app=""
while [ $# -gt 0 ]; do
  case "$1" in
    --remote) remote_arg="<string>--remote</string>" ;;
    --app) app=${2:?--app needs the path to Graviton.app}; shift ;;
    --uninstall) uninstall=1 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
  shift
done

if [ "${uninstall:-0}" = 1 ]; then
  launchctl bootout "$domain/$label" 2>/dev/null || true
  rm -f "$plist"
  echo "Removed the launch agent. The binary in $bin_dir is left in place."
  exit 0
fi

mkdir -p "$log_dir" "$(dirname "$plist")"
if [ -n "$app" ]; then
  # Already signed (and notarised) as part of the app; it finds its guest in the app.
  daemon="$app/Contents/MacOS/gravitond"
  [ -x "$daemon" ] || { echo "$daemon not found; build it with scripts/release.sh" >&2; exit 1; }
else
  cargo build --quiet --release --manifest-path "$repo/Cargo.toml" -p gravitond
  target_dir=$(cargo metadata --format-version 1 --no-deps --manifest-path "$repo/Cargo.toml" |
    python3 -I -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
  mkdir -p "$bin_dir"
  cp "$target_dir/release/gravitond" "$bin_dir/gravitond"
  codesign --force --sign - --entitlements "$repo/crates/gravitond/gravitond.entitlements" "$bin_dir/gravitond"
  daemon="$bin_dir/gravitond"
fi

cat > "$plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key><array><string>$daemon</string>$remote_arg</array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardErrorPath</key><string>$log_dir/gravitond.log</string>
  <key>StandardOutPath</key><string>$log_dir/gravitond.log</string>
</dict>
</plist>
PLIST

launchctl bootout "$domain/$label" 2>/dev/null || true
launchctl bootstrap "$domain" "$plist"
echo "gravitond is installed and running; it starts at login. Logs: $log_dir/gravitond.log"
