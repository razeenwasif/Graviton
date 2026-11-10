# Graviton

Native containers for macOS: a Rust engine that runs each Linux container in
its own lightweight VM (Apple Virtualization.framework), a `grav` CLI, and
SwiftUI apps for macOS and iOS.

> Status: **milestone 1 done**. Graviton boots a Linux guest VM on
> Virtualization.framework and runs commands in it through the agent (~0.3 s
> to agent ready). The daemon, CLI and both apps still show sample containers;
> pulling and running images is milestone 2.

## Layout

```
crates/
  graviton-proto   Wire types (control API + guest agent channel), JSON lines
  graviton-core    Image references, engine store, well-known paths
  graviton-vmm     VM config + Hypervisor trait; Virtualization.framework backend
  graviton-agent   PID 1 inside each guest VM: mounts, vsock modules, exec over vsock
  gravitond        Engine daemon, serves the control API on a Unix socket
  grav             CLI client
apps/
  project.yml      XcodeGen spec → Graviton.xcodeproj (generated, not committed)
  GravitonKit/     Shared Swift package: models, socket client, demo engine, theme, components
  Mac/             macOS app (Containers, Logs, Images + Pull sheet)
  iOS/             iOS companion (Containers, Detail, Logs)
scripts/
  build-guest.sh   Guest kernel (Alpine linux-virt) + initramfs with the agent as /init
  dev-gravitond.sh Build gravitond, sign it with the virtualization entitlement, run it
design/
  icon.svg, icon-macos.svg, make-icons.sh   App icon source → asset catalogs
```

The UI follows the mockups on the Graviton design canvas (dark, minimal, one
periwinkle accent `#7C8CFF`; green, amber and red only for status).

## Running it

```bash
cargo run -p gravitond          # serves sample containers on the default socket
cargo run -p grav -- ps -a      # in another terminal
cargo run -p grav -- logs api-gateway -n 5
```

Boot a guest VM and run a command in it (milestone 1):

```bash
scripts/build-guest.sh                         # once: kernel + initramfs
scripts/dev-gravitond.sh boot-check            # runs `uname -a` in the guest
scripts/dev-gravitond.sh boot-check --cpus 4 --memory 1024 -- /bin/sh -c 'nproc; free -m'
```

Virtualization.framework requires the `com.apple.security.virtualization`
entitlement, so gravitond must be signed (the script does it ad hoc). The guest
console is written to `~/Library/Application Support/Graviton/vms/boot-check/console.log`.

Socket: `$GRAVITON_SOCKET`, else `~/Library/Application Support/Graviton/gravitond.sock`.

Apps:

```bash
cd apps && xcodegen generate && open Graviton.xcodeproj
```

The macOS app connects to gravitond when it's running and falls back to demo
data otherwise (the sidebar card says which). The iOS app always uses demo
data until remote hosts land.

Tests:

```bash
cargo test --workspace
swift test --package-path apps/GravitonKit
```

## Protocol

One JSON object per line over the Unix socket:

```json
{"method":"logs","params":{"id":"api-gateway","tail":3}}
{"type":"logs","data":[{"seq":1282,"timestamp_ms":1762761600000,"level":"info","message":"..."}]}
```

`graviton-proto` is the source of truth; `GravitonKit/Wire.swift` mirrors it
and both sides have tests pinning the JSON shape.

## Roadmap

1. ~~**Boot a VM**~~ (done): Virtualization.framework backend via
   `objc2-virtualization`, Alpine kernel + initramfs, agent answers over vsock
   and runs commands, clean power-off. `gravitond boot-check` exercises it.
2. **Run an image**: registry client, content-addressed layer store, rootfs
   assembly, `grav pull` / `grav run`. Enables Run and Pull in the apps.
3. **Daemon features**: NAT networking, port forwarding, volumes, persistent
   state, launchd agent.
4. **Remote hosts**: authenticated TCP/TLS endpoint so the iOS app can pair
   with a Mac.
5. **Polish**: exec + interactive terminals, Files/Stats/Inspect tabs, image
   builds, app sandbox + XPC, signing and notarisation.
