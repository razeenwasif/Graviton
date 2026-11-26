# Graviton

Native Linux containers for Apple Silicon macOS — one lightweight `Virtualization.framework` VM per container, paired with the `grav` CLI, Docker Engine REST API compatibility, and native SwiftUI apps for macOS and iOS.

**Requirements:** Apple Silicon (`arm64`) Mac running **macOS 15+** (optional iOS companion requires **iOS 18+**).  
**Latest Release:** [`v0.3.0` on GitHub Releases](https://github.com/razeenwasif/Graviton/releases/latest)

## Highlights

- **VM-isolated containers:** Each container runs inside its own hardware-virtualized VM via Apple's `Virtualization.framework` (`objc2-virtualization`).
- **Ultra-lean Linux guest:** Purpose-built zero-module arm64 `Linux 6.12` kernel (`6.8 MiB`) and pure-Rust PID 1 initramfs (`554 KiB` compressed — no BusyBox or `mke2fs`).
- **Instant shared EROFS + `overlayfs` roots:** OCI images compile once into read-only EROFS disks (`/dev/vdb`) shared across containers over an APFS-cloned sparse `ext4` writable upper layer (`/dev/vda`), with zero per-container layer unpacking.
- **Adaptive host RAM reclaim:** Built-in `virtio-balloon` dynamically reclaims idle guest pages down to `64 MiB` minimum VM allocations.
- **Fast cold starts with `.vzsave` snapshots:** Lock-aware per-shape VM snapshot pool (up to 8 slots per `(cpus, memory, rosetta)` shape) with unique identity and live volume/clock/cache synchronization.
- **`linux/amd64` via Rosetta:** Seamless x86_64 execution through Apple Rosetta for Linux (`binfmt_misc` with `OCF` flags) when `--platform linux/amd64` is requested or required.
- **Docker Compose & Docker API compatible:** Run multi-service `compose.yaml` stacks natively with `grav compose`, or point standard `docker`, Compose, Dev Containers, and Testcontainers at Graviton's Docker Engine REST API v1.47 socket.
- **Native macOS & iOS apps:** Stratus-aligned pitch-black UI with command palette (`⌘K`), managing containers, live stats, logs, interactive terminals ([SwiftTerm](https://github.com/migueldeicaza/SwiftTerm)), files, volumes, networks, and macOS/iOS WidgetKit widgets locally or across paired Macs over pinned TLS.

---

## Installation

### Disk Image (Signed & Apple-Notarized DMG)

1. Download `Graviton-0.3.0.dmg` from [GitHub Releases](https://github.com/razeenwasif/Graviton/releases/latest).
2. Drag **Graviton.app** into **Applications**.
3. Start the background engine at login:

```bash
/Applications/Graviton.app/Contents/MacOS/grav service install
```

Update anytime from the CLI or in the Mac app (**Settings (`⌘,`) ▸ Updates**):

```bash
grav update          # check GitHub Releases for a newer version
grav update --check  # exit 1 if an update is available (for scripts)
```

### Homebrew Cask

Defined in [`Casks/graviton.rb`](Casks/graviton.rb):

```bash
brew tap razeenwasif/graviton https://github.com/razeenwasif/Graviton
brew install --cask razeenwasif/graviton/graviton
```

### Shell Completions

Generate completions for `zsh`, `bash`, or `fish` (no running daemon required):

```bash
# Zsh
mkdir -p "${ZDOTDIR:-$HOME}/.zsh/completions"
grav completions zsh > "${ZDOTDIR:-$HOME}/.zsh/completions/_grav"

# Bash
grav completions bash > "$(brew --prefix)/etc/bash_completion.d/grav"

# Fish
grav completions fish > ~/.config/fish/completions/grav.fish
```

---

## Quick Start

### Registry Authentication & Image Lifecycle

Log in to Docker Hub, GHCR, or any OCI registry (Bearer or Basic auth) via CLI or **Settings (`⌘,`)**. Usernames are stored in `~/Library/Application Support/Graviton/registries.json` (`0600`) and credentials stay in your macOS login Keychain.

```bash
grav login ghcr.io
grav login --list
grav pull alpine                           # streams per-layer progress (gzip, zstd, zstd:chunked)
grav tag myapp ghcr.io/me/myapp:1.0        # alias without copying layers
grav push ghcr.io/me/myapp:1.0             # push using saved Keychain credentials
grav save -o myapp.tar.gz myapp            # export image archive (or stdout)
grav load -i myapp.tar.gz                  # import image archive (or stdin)
grav commit web myapp:snapshot             # create a new image from a container's changes
grav logout ghcr.io
```

### Running & Managing Containers

```bash
# Ephemeral interactive shell
grav run -it --rm alpine sh

# Minimal-memory daemonized web server with port forwarding and bind mount
grav run -d --name web --memory 64 -p 8080:8000 -v ~/site:/srv python:3.13-slim \
    python3 -m http.server 8000 --directory /srv
curl localhost:8080

# Hardened container on an isolated network
grav run -d --name worker --read-only --cap-drop ALL --tmpfs /tmp --init --network app-net alpine sleep 3600

# Inspect, execute, transfer files, pause, and clean up
grav ps -a
grav logs -f web
grav exec -it web sh
grav stats web                             # live CPU, memory, network, PIDs (--no-stream, --format json)
grav top web
grav ls web /srv
grav cat web /etc/os-release
grav cp ./config.json web:/srv/            # copy local file/dir in (or `-` for tar stream)
grav cp web:/srv/config.json ./config-out.json
grav inspect web
grav pause web && grav unpause web
grav stop web && grav rm web
```

### Building Images (`grav build`)

Builds execute in a dedicated builder VM (4 vCPU, 4 GiB) with content-addressed step caching:

```bash
grav build -t myapp .
grav build -f docker/Dockerfile --build-arg PY=3.12 --target deps .
grav build --secret id=token,env=API_TOKEN -t myapp .
grav build --platform linux/arm64,linux/amd64 -t ghcr.io/me/myapp:1.0 .
```

**Supported Dockerfile instructions & features:**
`FROM` (multi-stage, `AS`, `ARG`s in `FROM`), `RUN` (`<<EOF` / `<<-EOF` heredocs, `#!` shebangs, inline shell heredocs, `--mount=type=cache|secret|bind|tmpfs`), `COPY` & `ADD` (local contexts, tarballs, heredocs, `ADD <http(s)://url>` with `--checksum`, wildcards, `--from`, `--chown`, `--chmod`, hard links preserved as tar links), `ONBUILD`, `HEALTHCHECK`, `CMD`, `ENTRYPOINT`, `SHELL`, `ENV`, `ARG`, `WORKDIR`, `USER`, `EXPOSE`, `LABEL`, `VOLUME`, `STOPSIGNAL`, and `.dockerignore`.  
*(Not yet supported: Git repository build URLs.)*

### Docker Compose Stacks (`grav compose`)

Run multi-service stacks from `compose.yaml` / `docker-compose.yml` (with automatic `compose.override.yaml`, multiple `-f` files, YAML merge keys `<<:`, `.env` + `env_file`, `${VAR}` interpolation, `--profile`, `build:`, named & bind volumes, `ports`, `cpus` / `mem_limit` / `deploy.resources.limits`, and topological `depends_on` including `service_started`, `service_completed_successfully`, and `service_healthy`):

```bash
grav compose up -d [--build] [--force-recreate]
grav compose ps -a
grav compose ls
grav compose logs -f api
grav compose config [--json]
grav compose down -v
```

Both the macOS and iOS apps group Compose services into collapsible **Stacks** with one-click Start, Stop, and Restart actions.

### Docker Engine REST API (`docker.sock`)

`gravitond` exposes a Docker Engine REST API v1.47 Unix socket at `~/Library/Application Support/Graviton/docker.sock` (override with `--docker-socket` / `$GRAVITON_DOCKER_SOCKET`, or disable with `--no-docker-socket`), enabling standard tools (`docker`, Compose, VS Code Dev Containers, Testcontainers, and Docker SDKs):

```bash
eval "$(grav docker-env)"   # exports DOCKER_HOST and DOCKER_BUILDKIT=0
docker ps -a
docker run --rm --label devcontainer.local_folder="$PWD" -v "$PWD":/workspace alpine:3.22 uname -a
```

Covers classic `POST /build` (`Builder-Version: 1`), `/_ping`, `/version`, `/info`, `/system/df`, `/events`, `/containers/*`, `/exec/*`, `/images/*` (including OCI metadata labels), `/volumes/*`, and `/networks/*`.

### Disk & VM Snapshot Management

```bash
grav df                         # breakdown of images, container disks, build cache, and snapshots
grav rmi alpine                 # refuses to remove images referenced by containers
grav prune                      # prune unreferenced build cache and layers (safety window: 10 min)
grav prune --images
grav prune -a --dry-run

grav snapshot status            # inspect cached .vzsave VM snapshots
grav snapshot warm              # pre-warm snapshot slots
grav snapshot clear             # drop cached snapshots (or disable via GRAVITON_NO_SNAPSHOTS=1)
```

---

## Architecture & Capabilities

### VM, Kernel & Storage

- **Custom Kernel & Rust Agent:** `scripts/build-kernel.sh` builds a zero-module `Linux 6.12` arm64 kernel (`scripts/kernel.config`, `6.8 MiB`) with `virtio`, `vsock`, `virtiofs`, `ext4`, `erofs`, `overlayfs`, `virtio-balloon`, `page_reporting`, and `binfmt_misc` compiled in. `initrd.img` (`554 KiB`) contains only the statically linked Rust `graviton-agent` binary as `/init`, handling pure-Rust `AF_PACKET` DHCPv4 + `NETLINK_ROUTE` networking, namespace setup, and zombie reaping (`PID 1`) without BusyBox or `mke2fs`.
- **Shared EROFS + `overlayfs` Storage:** OCI image layers compile once on the host (`crates/gravitond/src/erofs.rs`) into read-only uncompressed EROFS v1 images attached as `/dev/vdb`, mounted underneath an APFS-`clonefile`-cloned sparse `ext4` writable upper disk (`empty-disk.gvsp.gz`, `112 KiB`, up to `$GRAVITON_DISK_GIB`, default `64 GiB`) as `/dev/vda`. Container data persists across `grav stop`/`start`, and host page cache is shared across identical image containers.
- **Adaptive Memory Management:** Every VM attaches a `virtio-balloon` device managed by `gravitond`, reclaiming unused guest pages while subtracting `/proc/vmstat` inflate/deflate counters so reported container memory reflects real active guest usage (`MIN_MEMORY_MIB = 64`).
- **Snapshot Pool & Rosetta:** `gravitond` maintains up to 8 identity-unique `.vzsave` VM slots per `(cpus, memory, rosetta)` shape, restoring saved VM memory while dynamically re-binding `-v` directory shares, syncing UTC clocks, and invalidating stale block caches. Installing Apple Rosetta for Linux (`softwareupdate --install-rosetta`) enables `--platform linux/amd64` containers via `VZLinuxRosettaDirectoryShare` and `binfmt_misc` (`OCF` flags).

### Networking & Security

- **Container & User-Defined Networks:** Containers connect by default to the `graviton` bridge and can create/attach user-defined L2-isolated bridge networks without restarting running containers:

  ```bash
  grav network create app-net
  grav network connect app-net web
  grav network disconnect app-net web
  grav network ls
  grav network rm app-net
  ```

- **Scoped DNS & Port Publishing:** The guest DNS resolver (`127.0.0.11:53`) resolves containers and Compose service names/aliases (`<name>`, `<name>.graviton`) strictly within shared network boundaries, falling back to NAT DNS upstream. Publish TCP/UDP ports via `-p [IP:]HOST:CONTAINER[/tcp|udp]` (default `127.0.0.1`).
- **Runtime Hardening:** Supports `--read-only` rootfs, `--cap-drop` / `--cap-add`, `--tmpfs`, `--shm-size`, `--ulimit`, `--init` (`docker-init`), `HEALTHCHECK` (`--health-*` / `--no-healthcheck`), PID/mount/UTS/IPC namespace isolation, `--restart no|on-failure|always` (exponential backoff `1s`–`60s`), bounded rotating logs (`10 MiB`), and compacted stats history.

### Remote Macs & iOS Companion

Drive any Apple Silicon Mac running **Graviton.app** from another Mac (`grav --host` / Mac Host menu) or the **Graviton** iPhone app over certificate-pinned TLS (`_graviton._tcp` over Bonjour, TCP port `7843`):

**On the server Mac:**

```bash
xattr -dr com.apple.quarantine /Applications/Graviton.app   # if downloaded manually
/Applications/Graviton.app/Contents/MacOS/grav service install --remote
/Applications/Graviton.app/Contents/MacOS/grav pair           # prints code, fingerprint, and QR/URL
```

**On the client Mac:**

```bash
grav hosts add build-mac.local ABCD-EFGH --name studio      # or paste graviton://pair link
grav --host studio ps -a
grav hosts use studio                                         # set default host (`local` switches back)
grav hosts update studio 10.0.0.12                           # refresh saved endpoint/Bonjour metadata
grav hosts
grav devices                                                  # inspect/revoke paired devices on server
```

On iOS, pair by Bonjour discovery, manual host + code, `graviton://pair` link, or QR camera scan. The phone pins the Mac's TLS certificate and stores auth tokens in Keychain (`gravitond` stores only salted hashes). The iOS app provides Containers, Logs, Shell, Stats, Files, Inspect, an Activity feed (`events.jsonl`), APNs push notifications (`grav push setup|status|test`), and Home Screen (`small`, `medium`, `large`) + Lock Screen (`circular`, `rectangular`, `inline`) WidgetKit widgets backed by App Group `group.dev.graviton.shared` and `graviton://` deep links (`graviton://containers`, `graviton://container/<id>`, `graviton://images`, `graviton://resources`, `graviton://activity`, `graviton://hosts`).

To build `GravitonIOS` for your own device, set `GRAVITON_TEAM_ID` in `apps/Config/Signing.local.xcconfig` (`cp apps/Config/Signing.local.xcconfig.example apps/Config/Signing.local.xcconfig`):

```bash
cd apps && xcodegen generate
xcodebuild -project Graviton.xcodeproj -scheme GravitonIOS -destination generic/platform=iOS \
    -allowProvisioningUpdates -skipPackagePluginValidation build
xcrun devicectl device install app --device DEVICE_ID path/to/Graviton.app
```

### Optional AI Assistant

GravitonMac includes an **Ask Fairy (`⌥⌘I`)** panel that connects over a private `0600` Unix socket (authenticated with a per-turn 64-hex token) to a local Fairy assistant if installed. It exposes **31 tools** covering engine status, container/stack lifecycle, exec, logs, stats, file transfer, image builds/registry ops, volumes, networks, snapshots, and disk pruning — with automatic secret redaction in `inspect` responses and a tiered Touch ID (`LocalAuthentication`) approval policy for state-mutating or destructive operations.

---

## Repository Layout

```text
crates/
  graviton-proto    Wire protocol (JSON-lines control API, guest vsock channel, binary terminal & archive framing)
  graviton-core     Image references, Compose parser & dependency planner, engine store, paths
  graviton-image    OCI registry client (multi-arch index, gzip/zstd/zstd:chunked), blob store, Dockerfile parser
  graviton-vmm      VM configuration + Virtualization.framework hypervisor backend
  graviton-agent    Static Linux PID 1 guest agent (EROFS/overlayfs mount, namespaces, netlink/DHCPv4, builder)
  gravitond         Engine daemon (control API, Docker Engine REST v1.47 API, TLS remote host, snapshot/balloon loops)
  grav              CLI client
apps/
  project.yml       XcodeGen specification -> Graviton.xcodeproj
  Config/           Release, iOS, and local signing xcconfig templates
  GravitonKit/      Shared Swift package (models, socket/TLS client, demo engine, design tokens)
  Shared/           Cross-platform SwiftUI views and SwiftTerm terminal integration
  Mac/              macOS app (menu bar, Stratus UI, command palette, containers, compose stacks, builds, volumes, networks, Fairy panel)
  iOS/              iOS companion app (remote hosts, shell, stats, files, activity, APNs push alerts, deep links)
  Widget/           macOS & iOS WidgetKit extension (runtime status, container KPIs, Lock Screen widgets)
scripts/
  build-kernel.sh   Builds slim custom Linux 6.12 arm64 kernel inside Graviton and caches it
  kernel.config     Zero-module minimal arm64 kernel Kconfig
  build-guest.sh    Builds guest initrd.img, GVSP ext4 template, and bundles custom or --alpine kernel
  dev-gravitond.sh  Builds, signs (virtualization entitlement), and runs gravitond + boot-check
  install-launch-agent.sh  Installs/uninstalls gravitond LaunchAgent
  deploy-remote.sh  Deploys signed gravitond/grav/guest artifacts to a paired or local Mac
  release.sh        Builds, hardens, signs, notarizes, staples, and packages DMG/ZIP releases
Casks/
  graviton.rb       Homebrew Cask definition
design/             Graviton-cube app icon and menu bar vector sources + generator script
docs/               Architecture & status overview page
```

---

## Building from Source & Development

### Guest & Daemon

```bash
scripts/build-guest.sh              # builds initramfs + GVSP template; uses cached custom kernel (or --custom / --alpine)
scripts/dev-gravitond.sh            # builds, signs with com.apple.security.virtualization, and runs gravitond
scripts/dev-gravitond.sh --demo     # serves demo fixture data without booting VMs
```

Run a bare guest VM sanity check:

```bash
scripts/dev-gravitond.sh boot-check            # boots guest and runs `/init --uname`
scripts/dev-gravitond.sh boot-check --rosetta  # verifies Rosetta binfmt_misc registration
scripts/dev-gravitond.sh boot-check --snapshot # benchmarks VM snapshot save + restore
```

Console logs are written to `~/Library/Application Support/Graviton/vms/boot-check/console.log`.  
Default daemon sockets live at `~/Library/Application Support/Graviton/gravitond.sock` (`$GRAVITON_SOCKET`) and `docker.sock` (`$GRAVITON_DOCKER_SOCKET`).

### Apps & Tests

```bash
cd apps && xcodegen generate && open Graviton.xcodeproj
```

> **Note:** SwiftTerm is pinned to `1.11.2` (CoreText renderer). `xcodebuild` invocations require `-skipPackagePluginValidation` for SwiftTerm's version plugin.

Run tests:

```bash
cargo test --workspace --locked
swift test --package-path apps/GravitonKit
GRAVITON_LIVE=1 swift test --package-path apps/GravitonKit   # drives a live local gravitond
```

Deploy updated binaries/guest artifacts from source to `local` or another paired Mac without cutting a full release:

```bash
scripts/deploy-remote.sh studio --guest
scripts/deploy-remote.sh local --guest
```

### Wire Protocol

Clients communicate with `gravitond` over the Unix socket or pinned TLS using newline-delimited JSON (`crates/graviton-proto` matched by `GravitonKit/Wire.swift`):

```json
{"method":"logs","params":{"id":"api-gateway","tail":3}}
{"type":"logs","data":[{"seq":1282,"timestamp_ms":1762761600000,"level":"info","message":"..."}]}
```

- **Interactive PTY streams** switch to binary framing after `attached`: a 1-byte kind (`0` data, `1` resize, `2` EOF, `3` exit code), a big-endian `u32` length, and payload.
- **Archive transfers** (`get_archive` / `put_archive`) stream raw tar data across the same Unix socket or TLS connection without buffering through intermediate storage.

---

## Releasing & Licensing Notes

`scripts/release.sh` produces a hardened, Developer ID signed, Apple-notarized, and stapled `Graviton.app` alongside `dist/Graviton-<version>.dmg`, `.zip`, and `SHA256SUMS.txt`.

One-time release setup:

1. Ensure a **Developer ID Application** certificate is present in your login Keychain.
2. Configure your team ID in `apps/Config/Signing.local.xcconfig` (`cp apps/Config/Signing.local.xcconfig.example apps/Config/Signing.local.xcconfig`).
3. Store a `notarytool` keychain profile:

```bash
xcrun notarytool store-credentials graviton-notary --apple-id YOUR_APPLE_ID --team-id YOUR_TEAM_ID
```

Build release artifacts:

```bash
scripts/release.sh                # full Developer ID sign + notarize + staple + DMG/ZIP
scripts/release.sh --no-notarize  # Developer ID signing only
scripts/release.sh --adhoc        # local pipeline check without certificates
```

- **Guest kernel attribution:** The bundled guest kernel (`vmlinux`) is Linux 6.12 licensed under **GPL-2.0**. Source URLs and build recipe details are shipped inside the app bundle at `Contents/Resources/guest/SOURCES.txt`.
- **Distribution:** `Graviton.app` is currently built for **Developer ID** distribution without the macOS App Sandbox so the app can talk directly to `gravitond.sock` in `~/Library/Application Support/Graviton`.

---

## Backlog

- **iOS TestFlight distribution** (`GravitonIOS` via App Store Connect).
- **BuildKit remote cache** (`--cache-from` / `--cache-to` for OCI registry build caching).
- **Live resource updates** (hot `--cpus` and `--memory` adjustments on running containers).
- **Fairy assistant tool parity** (`grav network`, `--shm-size`, and `--ulimit` parameters).
- **App Sandbox & XPC service** for optional Mac App Store distribution.
