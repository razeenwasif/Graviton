#!/bin/sh
# Install gravitond as a launch agent, so it starts at login and restarts if it
# crashes. The signed binary goes to ~/Library/Application Support/Graviton/bin
# and logs to ~/Library/Logs/Graviton/gravitond.log.
#   scripts/install-launch-agent.sh            # install (or update) and start
#   scripts/install-launch-agent.sh --uninstall
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
label=dev.graviton.gravitond
plist="$HOME/Library/LaunchAgents/$label.plist"
bin_dir="$HOME/Library/Application Support/Graviton/bin"
log_dir="$HOME/Library/Logs/Graviton"
domain="gui/$(id -u)"

if [ "${1:-}" = "--uninstall" ]; then
  launchctl bootout "$domain/$label" 2>/dev/null || true
  rm -f "$plist"
  echo "Removed the launch agent. The binary in $bin_dir is left in place."
  exit 0
fi

cargo build --quiet --release --manifest-path "$repo/Cargo.toml" -p gravitond
target_dir=$(cargo metadata --format-version 1 --no-deps --manifest-path "$repo/Cargo.toml" |
  python3 -I -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
mkdir -p "$bin_dir" "$log_dir" "$(dirname "$plist")"
cp "$target_dir/release/gravitond" "$bin_dir/gravitond"
codesign --force --sign - --entitlements "$repo/crates/gravitond/gravitond.entitlements" "$bin_dir/gravitond"

cat > "$plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key><array><string>$bin_dir/gravitond</string></array>
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
