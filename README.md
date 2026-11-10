# Graviton

Native containers for macOS: a Rust engine that runs each Linux container in
its own lightweight VM (Apple Virtualization.framework), a `grav` CLI, and
SwiftUI apps for macOS and iOS.

> Status: **milestone 3 done**. Containers run from real images, each in its
> own VM (`grav run alpine` takes about half a second end to end), with NAT
> networking, published ports, folder and named volumes, `grav exec` (including
> `-it` shells), and containers and logs saved across restarts. The Mac app
> pulls, runs and removes them. Next: remote hosts for the iPhone app.

## Layout

```
crates/
  graviton-proto   Wire types (control API + guest agent channel), JSON lines
  graviton-core    Image references, engine store, well-known paths
  graviton-image   Registry client, content-addressed blob store, local image index
  graviton-vmm     VM config + Hypervisor trait; Virtualization.framework backend
  graviton-agent   PID 1 inside each guest VM: unpacks images, runs containers, talks vsock
  gravitond        Engine daemon: control API on a Unix socket, one VM per container
  grav             CLI client
apps/
  project.yml      XcodeGen spec → Graviton.xcodeproj (generated, not committed)
  GravitonKit/     Shared Swift package: models, socket client, demo engine, theme, components
  Mac/             macOS app (Containers, Run sheet, Logs, Images + Pull sheet)
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
scripts/build-guest.sh                      # once: guest kernel + initramfs
scripts/dev-gravitond.sh                    # build, sign and run the daemon
cargo run -p grav -- pull alpine            # in another terminal
cargo run -p grav -- run alpine cat /etc/os-release
cargo run -p grav -- run -d --name web -p 8080:8000 -v ~/site:/srv python:3.13-slim \
    python3 -m http.server 8000 --directory /srv
curl localhost:8080
cargo run -p grav -- ps -a
cargo run -p grav -- logs web
cargo run -p grav -- exec web python3 --version
cargo run -p grav -- exec -it web sh
cargo run -p grav -- stop web && cargo run -p grav -- rm web
```

- `-p HOST:CONTAINER` publishes a TCP port on 127.0.0.1 (forwarded to the
  container's VM on Virtualization.framework's NAT network).
- `-v /mac/dir:/path[:ro]` shares a folder over virtiofs; `-v name:/path` is a
  named volume under `~/Library/Application Support/Graviton/volumes`.
- Containers, their settings and logs are saved; after gravitond restarts,
  running ones show as stopped and `grav start` brings them back.
- `scripts/install-launch-agent.sh` installs gravitond as a launch agent so it
  starts at login (`--uninstall` removes it).

`grav run` pulls the image if needed, boots a VM (2 vCPU, 1 GiB by default;
`--cpus`, `--memory`), unpacks the image inside it, runs the entrypoint and
streams the output; the exit code is the container's. `gravitond --demo`
serves the sample containers from the design mockups instead.

Boot a bare guest VM and run a command in it (milestone 1's check):

```bash
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
GRAVITON_LIVE=1 swift test --package-path apps/GravitonKit   # also drives a running gravitond
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
2. ~~**Run an image**~~ (done): registry client (token auth, multi-arch
   indexes, verified downloads), content-addressed blob store shared into the
   guest over virtiofs, layers unpacked in the guest with OCI whiteouts, the
   process run chrooted in new mount/UTS/IPC namespaces as the image's user,
   `grav pull` / `run` / `stop` / `rm`, and Pull, Run and Remove in the Mac app.
3. ~~**Daemon features**~~ (done): NAT networking with DHCP and DNS, `-p`
   port forwarding, `-v` folders and named volumes, saved state and logs,
   `grav exec` with `-it` terminals, a launch agent script.
4. **Remote hosts**: authenticated TCP/TLS endpoint so the iOS app can pair
   with a Mac.
5. **Polish**: exec + interactive terminals, Files/Stats/Inspect tabs, image
   builds, app sandbox + XPC, signing and notarisation.
