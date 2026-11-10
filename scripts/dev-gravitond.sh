#!/bin/sh
# Build gravitond, sign it ad hoc with the virtualization entitlement (required
# by Virtualization.framework), and run it with the given arguments.
#   scripts/dev-gravitond.sh                 # serve the control API
#   scripts/dev-gravitond.sh boot-check      # boot a guest VM and run a command
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
profile=${PROFILE:-debug}
flag=""; [ "$profile" = release ] && flag="--release"
cargo build --quiet --manifest-path "$repo/Cargo.toml" -p gravitond $flag
target_dir=$(cargo metadata --format-version 1 --no-deps --manifest-path "$repo/Cargo.toml" |
  python3 -I -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
bin="$target_dir/$profile/gravitond"
codesign --force --sign - --entitlements "$repo/crates/gravitond/gravitond.entitlements" "$bin" 2>/dev/null
exec "$bin" "$@"
