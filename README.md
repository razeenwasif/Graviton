# Graviton

Native containers for macOS: a Rust engine that runs each Linux container in
its own lightweight VM (Apple Virtualization.framework), a `grav` CLI, and
SwiftUI apps for macOS and iOS.

> Status: **scaffold**. The daemon, CLI, wire protocol and both apps work
> end to end against sample data. Booting VMs and pulling images are the next
> milestones.

## Layout

```
crates/
  graviton-proto   Wire types (control API + guest agent channel), JSON lines
  graviton-core    Image references, engine store, well-known paths
  graviton-vmm     VM config + Hypervisor trait; Virtualization.framework backend (stub)
  graviton-agent   PID 1 inside each guest VM (Linux only)
  gravitond        Engine daemon, serves the control API on a Unix socket
  grav             CLI client
apps/
  project.yml      XcodeGen spec → Graviton.xcodeproj (generated, not committed)
  GravitonKit/     Shared Swift package: models, socket client, demo engine, theme, components
  Mac/             macOS app (Containers, Logs, Images + Pull sheet)
  iOS/             iOS companion (Containers, Detail, Logs)
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

1. **Boot a VM**: Virtualization.framework backend in `graviton-vmm` (via
   `objc2-virtualization`), kernel + initramfs, agent says `ready` over vsock.
   gravitond needs the `com.apple.security.virtualization` entitlement.
2. **Run an image**: registry client, content-addressed layer store, rootfs
   assembly, `grav pull` / `grav run`. Enables Run and Pull in the apps.
3. **Daemon features**: NAT networking, port forwarding, volumes, persistent
   state, launchd agent.
4. **Remote hosts**: authenticated TCP/TLS endpoint so the iOS app can pair
   with a Mac.
5. **Polish**: exec + interactive terminals, Files/Stats/Inspect tabs, image
   builds, app sandbox + XPC, signing and notarisation.
