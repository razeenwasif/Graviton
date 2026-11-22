# Graviton

Native containers for macOS: a Rust engine that runs each Linux container in
its own lightweight VM (Apple Virtualization.framework), a `grav` CLI, and
SwiftUI apps for macOS and iOS.

> Status: **milestone 4 done, milestone 5 under way**. Containers run from real
> images, each in its own VM (`grav run alpine` takes about half a second end
> to end), with NAT networking, published ports, folder and named volumes,
> `grav exec` (including `-it` shells that follow window resizes and return
> the exit code), and containers and logs saved across restarts. The Mac app
> pulls, runs and removes them, and shows each one's live CPU, memory and
> network charts, processes, files and configuration, with a shell in a
> Terminal tab. `grav build` (and Build… in the Mac app) builds images from
> Dockerfiles, multi-stage included, with a layer cache. The iPhone app pairs
> with a Mac over TLS to show and control its containers: it runs containers
> and pulls images, opens shells, shows Stats, Files and Inspect, keeps an
> Activity feed of what happened on each Mac, and gets push alerts when a
> container fails or a build finishes. `grav --host` and the Mac app's Host
> menu do the same between Macs.

## Layout

```
crates/
  graviton-proto   Wire types (control API + guest agent channel), JSON lines
  graviton-core    Image references, Compose parser & dependency planner, engine store, well-known paths
  graviton-image   Registry client, blob store, local image index, Dockerfile parser
  graviton-vmm     VM config + Hypervisor trait; Virtualization.framework backend
  graviton-agent   PID 1 inside each guest VM: unpacks images, runs containers, writes build layers
  gravitond        Engine daemon: control API on a Unix socket, one VM per container
  grav             CLI client
apps/
  project.yml      XcodeGen spec → Graviton.xcodeproj (generated, not committed)
  GravitonKit/     Shared Swift package: models, socket client, demo engine, theme, components
  Shared/          Swift shared by both apps: shell sessions drawn by SwiftTerm
  Mac/             macOS app (Containers, Run sheet, Logs, Terminal, Stats, Files, Inspect, Images + Pull/Build/Clean Up sheets, Volumes, Networks, Fairy AI panel, menu bar item)
  iOS/             iOS companion (Containers, Detail, Logs, Shell, Stats, Files, Inspect, Run + Pull sheets, Activity, Hosts)
scripts/
  build-kernel.sh  Build custom Linux 6.12-graviton arm64 kernel (zero modules, ~8.2 MiB) inside Graviton
  kernel.config    Minimal arm64 Kconfig (virtio, vsock, virtiofs, ext4, overlayfs, balloon, binfmt_misc)
  build-guest.sh   Guest kernel (custom 6.12-graviton if cached, else Alpine linux-virt) + initramfs with agent as /init
  dev-gravitond.sh Build gravitond, sign it with the virtualization entitlement, run it
design/
  icon.svg, icon-macos.svg, make-icons.sh   App icon source → asset catalogs
  menubar.svg      Menu bar glyph (template image; drawn natively by Mac/MenuBarGlyph.swift)
```

The UI follows the mockups on the Graviton design canvas (dark, minimal, one
periwinkle accent `#7C8CFF`; green, amber and red only for status).

## Running it

```bash
scripts/build-guest.sh                      # once: guest kernel + initramfs
scripts/dev-gravitond.sh                    # build, sign and run the daemon
cargo run -p grav -- login ghcr.io          # optional: log in to a private registry (Keychain)
cargo run -p grav -- pull alpine            # in another terminal (streams per-layer progress)
cargo run -p grav -- tag myapp ghcr.io/me/myapp:1.0   # alias an image without copying layers
cargo run -p grav -- push ghcr.io/me/myapp:1.0        # push a local image (uses `grav login` creds)
cargo run -p grav -- run alpine cat /etc/os-release
cargo run -p grav -- run -it --rm alpine sh     # interactive PTY shell; stops & removes on exit
cargo run -p grav -- run -d --name web -p 8080:8000 -v ~/site:/srv python:3.13-slim \
    python3 -m http.server 8000 --directory /srv
curl localhost:8080
cargo run -p grav -- ps -a
cargo run -p grav -- logs web
cargo run -p grav -- exec web python3 --version
cargo run -p grav -- exec -it web sh
cargo run -p grav -- cp ./notes.txt web:/tmp/   # copy files or folders in or out (`-` for stdout tar)
cargo run -p grav -- cp web:/tmp/notes.txt ./notes-copy.txt
cargo run -p grav -- stop web && cargo run -p grav -- rm web
cargo run -p grav -- compose up -d              # multi-service stack from compose.yaml / docker-compose.yml
cargo run -p grav -- compose ps -a              # services in this stack (or `grav compose ls` for all stacks)
cargo run -p grav -- compose logs -f api        # multiplexed color-coded service logs
cargo run -p grav -- compose down -v            # tear down stack and project-owned named volumes
```

- `grav login [REGISTRY]` / `grav logout [REGISTRY]` authenticate with Docker
  Hub, GHCR or any OCI registry (Bearer or Basic auth). Usernames live in
  `~/Library/Application Support/Graviton/registries.json` (`0600`) and
  passwords/tokens stay in the Mac's login Keychain (`grav login --list` shows
  saved logins; `--password-stdin` reads from stdin). Both `gzip` and `zstd`
  image layers (including multi-frame and skippable-frame `zstd:chunked`) are
  supported, `grav pull` plus the Mac and iPhone Pull sheets stream per-layer
  progress, `grav tag SRC DST` aliases a local image without copying layers,
  and `grav push IMAGE` uploads images to OCI registries using the saved
  `grav login` credentials (`grav push setup|status|test` remains for iPhone
  push alerts).
- `-p [IP:]HOST:CONTAINER[/tcp|udp]` (or `-p PORT[/udp]`) publishes a TCP or
  UDP port forwarded to the container's VM. Without `IP:`, it binds `127.0.0.1`;
  pass `0.0.0.0:8080:80` (or `[::]:8080:80` / a specific interface IP) to accept
  connections from other machines on the network.
- Containers on the same Mac reach each other directly over the container
  network (TCP, UDP and ICMP) and resolve each other by name, `<name>.graviton`
  or container ID via the guest's embedded DNS resolver (`127.0.0.11:53`),
  falling back to the NAT's DNS for external hosts. `grav network` (and the Mac
  app's **Networks** screen) shows the live switch, DNS resolver, container IPs,
  and published TCP/UDP ports.
- `-v /mac/dir:/path[:ro]` shares a folder over virtiofs; `-v name:/path` is a
  named volume under `~/Library/Application Support/Graviton/volumes`. Manage
  named volumes with `grav volumes` (`grav volume create NAME`, `grav volume rm NAME`)
  or the Mac app's **Volumes** screen (size, file count, attached containers,
  bind mounts, and removal of unused volumes).
- Containers, their settings and logs are saved. Pass `--restart no|on-failure|always`
  (or pick **Restart** in the Mac/iPhone Run sheet) to automatically restart a
  container on exit with exponential backoff (1 s to 60 s) and bring it back
  when `gravitond` starts (after a deploy or reboot); containers with the
  default `no` stay stopped until `grav start`.
- `grav build` supports multi-stage Dockerfiles, heredocs (`RUN <<EOF` /
  `RUN <<-EOF` with optional `#!` shebangs, inline shell heredocs, and
  `COPY <<EOF` / `<<'EOF'`), `ADD <http(s)://url>` (with `--checksum`),
  `--chmod` and `--chown`, persistent `RUN --mount=type=cache`, ephemeral
  `RUN --mount=type=secret` (via `grav build --secret id=...,src=...` or
  `env=...`), `type=bind` and `type=tmpfs` mounts, and hard links preserved as
  tar link entries in layers. `grav df` counts persistent cache mounts and
  `grav prune` reclaims them.
- `scripts/build-kernel.sh` builds Graviton's purpose-built arm64 Linux 6.12
  kernel (`scripts/kernel.config`, zero loadable modules, all virtio/vsock/virtiofs/ext4/overlayfs/balloon/binfmt_misc
  drivers compiled in, ~8.2 MiB) inside a Graviton container and caches it for
  `scripts/build-guest.sh` (which uses it automatically when cached, `--custom`
  to build it, or `--alpine` for Alpine's `linux-virt` fallback). Every VM also
  attaches a `virtio-balloon` device for host RAM reclaim.
- **Rosetta for `linux/amd64` images**: when Apple Rosetta for Linux is
  installed (`softwareupdate --install-rosetta`), `grav pull`, `grav run`, and
  `grav build` accept `--platform linux/amd64` (also pickable in the Mac Pull
  sheet), and `amd64`-only images fall back to Rosetta automatically. The guest
  mounts `VZLinuxRosettaDirectoryShare` and registers `binfmt_misc` with `OCF`
  flags so x86_64 binaries execute directly inside chrooted containers.
- **VM snapshots**: `gravitond` caches saved `.vzsave` VM states in a per-shape
  slot pool (up to 8 unique-identity slots per `(cpus, memory, rosetta)` shape
  with distinct pinned MACs, dynamic `-v` volumes via `VZMultipleDirectoryShare`,
  host UTC clock sync, and block-cache invalidation across container restarts).
  Manage snapshots with `grav snapshot [status|warm|clear]` (or disable with
  `GRAVITON_NO_SNAPSHOTS=1`); `grav df` reports snapshot disk usage and
  `grav prune` clears idle snapshots.
- **Docker Compose & Stacks (`grav compose`)**: run multi-service stacks from
  `compose.yaml` / `docker-compose.yml` (with automatic `compose.override.yaml`,
  multiple `-f` files, YAML merge keys `<<:`, `.env` + `env_file`, `${VAR}`
  interpolation, `--profile`, `build:` contexts, named & bind volumes,
  `ports`, `entrypoint`, `working_dir`, `user`, `hostname`, `labels`, `cpus` /
  `mem_limit` / `deploy.resources.limits`, and `depends_on` topological
  ordering including `condition: service_completed_successfully`). Services in
  the same stack automatically resolve each other by service name (e.g. `db`,
  `db.graviton`) and `networks.<net>.aliases` without colliding across separate
  projects. Commands: `up [-d] [--build] [--no-build] [--force-recreate]`,
  `down [-v]`, `ps [-a]`, `logs [-f] [-n N]`, `stop`, `start`, `restart`,
  `build [--no-cache]`, offline `config [--json]`, and `ls`. Both the Mac and
  iPhone apps group Compose containers into collapsible **Stacks** with
  one-click stack Start/Stop/Restart controls and show Stack/Service/Labels in
  Inspect.
- **Fairy AI agent (`⌥⌘I`)**: open **Ask Fairy** from the Mac app's sidebar
  or press `⌥⌘I` to inspect and drive Graviton through your local `Fairy.app`
  (works with both full Fairy and Fairy Face) across both this Mac and paired
  remote Macs (such as `mini`). Graviton connects to Fairy over
  `~/Library/Application Support/lyra-companion/lyra/graviton.sock`, issues a
  64-hex-char per-turn token on its private `0600` socket
  (`~/Library/Application Support/Graviton/fairy-tools.sock`), saves
  conversations locally in `~/Library/Application Support/Graviton/fairy-history.json`
  (`0600`) with past-conversation search, resume, transcript copy, and delete
  controls, speaks replies aloud by default (via `gemini-4-neon-high`), exposes
  25 tools (status, events, host switching, UI navigation, containers, inspect
  with secret redaction, logs, stats, container/stack actions, `run`, `exec`,
  files/copy, images, pull, build, volumes, networks, Compose, `df`/`prune`,
  and VM snapshots), and enforces a **tiered Touch ID policy** inside Graviton
  (`LocalAuthentication` + inline Approval Card; fail-closed for unknown actions):
  - **Auto-run (no Touch ID):** all read queries plus safe reversible ops
    (`start`/`restart` container or stack, `compose start`/`restart`/`config`/`ps`/`logs`,
    `pull`, `volume_action create`, `disk_and_prune usage`/`prune_dry_run`,
    `snapshots status`, `switch_host`, and `navigate`).
  - **Touch ID required:** `stop`/`remove` container, `stop` stack, `run`,
    `exec`, `build`, `remove_image`, `remove` volume, `copy_files`,
    `disk_and_prune prune`, `snapshots warm`/`clear`, and
    `compose up`/`down`/`stop`/`build`.
- **Docker Engine REST API compatibility (`docker.sock`)**: `gravitond` also
  exposes a Docker Engine REST API v1.47 Unix socket at
  `$GRAVITON_DOCKER_SOCKET` (else `~/Library/Application Support/Graviton/docker.sock`;
  pass `--docker-socket PATH` or `--no-docker-socket` to override/disable) so
  standard tools (`docker` CLI, VS Code / Proton Dev Containers,
  Testcontainers, and Docker SDKs) drive Graviton VMs directly:

  ```bash
  eval "$(grav docker-env)"   # exports DOCKER_HOST and DOCKER_BUILDKIT=0 (classic POST /build)
  docker ps -a
  docker run --rm --label devcontainer.local_folder="$PWD" -v "$PWD":/workspace alpine:3.22 uname -a
  ```

  Includes classic `POST /build` (`Builder-Version: 1`), `GET /_ping`, `/version`,
  `/info`, `/system/df`, `/events`, `/containers/*` (with Go map & array label
  filters, `HostConfig.Binds` + `HostConfig.Mounts`, `AutoRemove`, `wait`,
  `logs`, TTY & multiplexed `attach`, `archive` `docker cp`), `/exec/*`
  (interactive TTY resize, streaming non-TTY `exec -i` stdin + EOF, `--workdir`
  and `--user` overrides also exposed via `grav exec -w/-u`), `/images/*`
  (inspect with full OCI labels including `devcontainer.metadata`, `tag`,
  `pull`, `push`, `rmi`, `prune`), `/volumes/*`, and `/networks/*`.
- `scripts/install-launch-agent.sh` installs gravitond as a launch agent so it
  starts at login (`--remote` to accept phones, `--uninstall` to remove it).

Pair the iPhone app:

```bash
scripts/dev-gravitond.sh --remote    # TLS on port 7843, advertised over Bonjour
cargo run -p grav -- pair            # one-time code, address, fingerprint, QR code
cargo run -p grav -- devices         # paired phones; `devices revoke ID` forgets one
```

On the phone, open Hosts and pick the Mac (or add it by address, paste the
link, or scan the QR code with the Camera), enter the code and check the
fingerprint matches. The phone pins the Mac's certificate and keeps its token
in the Keychain; gravitond stores only token hashes, in
`~/Library/Application Support/Graviton/remote/`.

`grav run` pulls the image if needed, boots a VM (2 vCPU, 1 GiB by default;
`--cpus`, `--memory`), unpacks the image inside it, runs the entrypoint and
streams the output; the exit code is the container's. `gravitond --demo`
serves the sample containers from the design mockups instead.

The container's files live on its own disk, not in the VM's memory: a sparse
disk image (`disk.img` beside its console log, up to `$GRAVITON_DISK_GIB`,
default 64 GiB, using only what's written) that the agent formats ext4 on the
container's first start. So an image and what runs in it can be far bigger
than `--memory`. The disk stays until `grav rm`: like `docker start`, a
stopped container starts again with the files it changed, and without
unpacking its image again (a few hundred ms instead of seconds for big
images). The image is unpacked once, flushed to disk before the container
runs, and a small journal keeps the disk consistent if a container has to be
powered off. `grav info` counts the disks' real size. Two containers of an image that isn't
here yet share one pull. The agent is PID 1, so it also waits for processes
orphaned inside the container, which would otherwise stay zombies.

Disk space:

```bash
grav df                         # images, container disks, build cache, and what each frees
grav rmi busybox                # remove an image no container uses
grav prune                      # build cache and layers nothing refers to
grav prune --images             # ... and images no container uses
grav prune -a --dry-run         # everything, including stopped containers; show only
```

Layers are shared: one that another image, a container or the build cache
still needs stays, and `grav rmi` refuses an image a container uses (it names
them). Layers written in the last 10 minutes are kept, so a pull or build in
progress isn't cut short. The Mac app's Images screen has **Clean Up…**, which
previews the same choices before freeing anything, and **Remove…** on each image.

Boot a bare guest VM and run a command in it (milestone 1's check):

```bash
scripts/dev-gravitond.sh boot-check            # runs `uname -a` in the guest
scripts/dev-gravitond.sh boot-check --rosetta  # verify Rosetta binfmt_misc registration
scripts/dev-gravitond.sh boot-check --snapshot # benchmark VM snapshot save + restore
scripts/dev-gravitond.sh boot-check --cpus 4 --memory 1024 -- /bin/sh -c 'nproc; free -m'
```

Virtualization.framework requires the `com.apple.security.virtualization`
entitlement, so gravitond must be signed (the script does it ad hoc). The guest
console is written to `~/Library/Application Support/Graviton/vms/boot-check/console.log`.

Socket: `$GRAVITON_SOCKET`, else `~/Library/Application Support/Graviton/gravitond.sock`
(and `$GRAVITON_DOCKER_SOCKET`, else `docker.sock` alongside it).

Apps:

```bash
cd apps && xcodegen generate && open Graviton.xcodeproj
```

SwiftTerm runs a small build plugin that records its version: Xcode asks you
to trust it once; `xcodebuild` needs `-skipPackagePluginValidation`. It's
pinned to 1.11.2, the last release that draws with CoreText; later releases
need Xcode's Metal toolchain.

The macOS app connects to gravitond when it's running and falls back to demo
data otherwise (the sidebar card says which). The iOS app shows demo data
until you pair it with a Mac.

Tests:

```bash
cargo test --workspace
swift test --package-path apps/GravitonKit
GRAVITON_LIVE=1 swift test --package-path apps/GravitonKit   # also drives a running gravitond
```

## The iPhone app

Pair it with each Mac (see above), then: + on Containers runs a container,
Images pulls one, and a container's page has Logs, Shell, Stats, Files and
Inspect. Activity lists what each Mac's engine did (started, stopped, failed,
pulled, built, removed), kept in `events.jsonl`, the last 500 events; `grav
events [-n N] [-f]` shows the same feed in a terminal.

Push alerts, for a container that fails and a build that finishes or fails,
go through Apple's push service with your team's APNs key (a `.p8` from
Certificates, Identifiers & Profiles > Keys). Set it up on each Mac, on that
Mac itself:

```bash
grav --host local push setup --key-id KEYID --team-id TEAMID AuthKey_KEYID.p8
grav --host local push status    # on or off, and how many phones are registered
grav --host local push test      # one alert to every registered phone
```

gravitond checks the key and keeps it in the login Keychain (service
`dev.graviton.apns`); only the two ids go in `push.json`. The phone registers
for alerts with each Mac it's paired with once you allow notifications.

To build the app for your own iPhone, put your team ID in
`apps/Config/Signing.local.xcconfig` (as for releases); `apps/Config/iOS.xcconfig`
signs automatically with it and the push entitlement:

```bash
cd apps && xcodegen generate
xcodebuild -project Graviton.xcodeproj -scheme GravitonIOS -destination generic/platform=iOS \
    -allowProvisioningUpdates -skipPackagePluginValidation build
xcrun devicectl device install app --device DEVICE_ID path/to/Graviton.app
```

## Using another Mac

Containers can run on any Apple-silicon Mac with Graviton.app; this one then
drives it over TLS, pinned to that Mac's certificate.

On the other Mac (once):

```bash
xattr -dr com.apple.quarantine /Applications/Graviton.app   # if it came by AirDrop or a download
/Applications/Graviton.app/Contents/MacOS/grav service install --remote
/Applications/Graviton.app/Contents/MacOS/grav pair           # a code, its address and fingerprint, a link
```

On this Mac:

```bash
grav hosts add mini.local ABCD-EFGH --name mini   # or paste the graviton://pair link
grav --host mini ps                               # one command there
grav hosts use mini                               # or make it the default (--host local for this Mac)
grav hosts                                        # paired Macs; grav hosts rm NAME forgets one
```

Everything works against a paired Mac, `exec -it` shells included.
`grav build` sends the folder there first (minus what `.dockerignore` leaves
out), so you build local projects on the other Mac. Volume paths (`-v
~/code:/app`) are paths on that Mac. Pairings live in `hosts.json` in
Graviton's support folder (readable only by you) and are shared with the Mac
app, whose Host menu (⌥⌘0 for this Mac, ⌥⌘1… for the others) and sidebar
switch which Mac the window shows; it can pair too. The app and `grav` share
one default: `grav hosts use mini` makes the app open on the mini, and picking
a Mac in the Host menu changes grav's default. So with the mini as the
default, this Mac is just the interface: containers, images and builds from
the app or `grav` all go to the mini. The iPhone app reopens on whichever Mac
you last picked in its Hosts tab.

`grav service install|uninstall|status` runs the gravitond inside Graviton.app
at login on any Mac, without the source.

## Updating another Mac

To update the engine on a Mac that runs Graviton.app as a server (the mini,
say) straight from this checkout, without a release:

```bash
scripts/deploy-remote.sh mini            # gravitond and grav
scripts/deploy-remote.sh mini --guest    # and the guest kernel and initramfs
scripts/deploy-remote.sh local --guest   # this Mac's Graviton.app, the same way
```

It builds both binaries, signs them with your Apple Development certificate
(`GRAVITON_SIGN_IDENTITY` picks another), copies them over, and swaps each in
as a new file before restarting gravitond there. If the new one doesn't
answer, the old files go back. It refuses while containers are running
(`--force` restarts anyway, stopping them). The certificate matters: the
Keychain on that Mac trusts the signature that stored the APNs key, so an ad
hoc build, whose identity changes every time, makes it ask on that Mac's
screen again. With a certificate it asks once (click Always Allow) and keeps
trusting later deploys. Replacing a binary in place instead of as a new file
gets it killed at launch, because macOS caches signatures by file.

## Releasing

`scripts/release.sh` builds a distributable Graviton.app: the app with
`gravitond`, `grav` and the guest kernel and initramfs inside it, signed
inside out with the hardened runtime (gravitond keeps its virtualization
entitlement), notarised, stapled, and packaged as a zip and a disk image in
`dist/`.

One-time setup, with your Apple Developer account:

1. A **Developer ID Application** certificate in your login keychain: Xcode >
   Settings > Accounts > Manage Certificates > + (the account holder may need
   to create it). An Apple Development certificate isn't enough.
2. Your team ID in `apps/Config/Signing.local.xcconfig` (ignored by git):

   ```bash
   cp apps/Config/Signing.local.xcconfig.example apps/Config/Signing.local.xcconfig
   ```

3. A notarytool profile in your keychain. It asks for an app-specific
   password (appleid.apple.com > Sign-In and Security) and keeps it there:

   ```bash
   xcrun notarytool store-credentials graviton-notary --apple-id YOUR_APPLE_ID --team-id YOUR_TEAM_ID
   ```

Then:

```bash
scripts/release.sh                # sign, notarise, staple, package
scripts/release.sh --no-notarize  # Developer ID signing only
scripts/release.sh --adhoc        # no certificate: checks the whole pipeline locally
```

To run the released engine at login: `scripts/install-launch-agent.sh --app
/Applications/Graviton.app` (add `--remote` for phones). gravitond running
from the app uses the app's own guest. Archiving GravitonMac in Xcode uses the
same Developer ID settings (`apps/Config/Release.xcconfig`); Debug builds stay
ad hoc.

The guest bundles Linux and BusyBox from Alpine (GPL-2.0); the app carries
`Resources/guest/SOURCES.txt` saying where their source is.

Not sandboxed yet: the app reaches gravitond's socket in `~/Library/Application
Support/Graviton`, which a sandboxed app can't. Developer ID distribution
doesn't need the sandbox; the App Store would, with the socket moved to an app
group container or an XPC service.

## Protocol

One JSON object per line over the Unix socket:

```json
{"method":"logs","params":{"id":"api-gateway","tail":3}}
{"type":"logs","data":[{"seq":1282,"timestamp_ms":1762761600000,"level":"info","message":"..."}]}
```

`graviton-proto` is the source of truth; `GravitonKit/Wire.swift` mirrors it
and both sides have tests pinning the JSON shape.

A terminal (`run` or `exec` with `"tty": true`) answers `attached`, then the
connection carries binary frames both ways: a kind byte (0 data, 1 resize, 2
end of input, 3 exit code), a big-endian u32 length and the payload. File
transfers (`get_archive` and `put_archive`) stream raw tar archives over the
same Unix socket or TLS channel (used by `grav cp` and the Mac app's Files tab,
including Finder drag-and-drop). gravitond passes them between the client and
the guest agent untouched, over the Unix socket or TLS.

Build images from Dockerfiles:

```bash
cargo run -p grav -- build -t myapp .              # ./Dockerfile; the image is myapp:latest
cargo run -p grav -- build -f docker/Dockerfile --build-arg PY=3.12 --target deps .
cargo run -p grav -- build --secret id=token,env=API_TOKEN -t myapp .
cargo run -p grav -- run myapp
```

Supported: FROM (multi-stage, `AS name`, ARGs in FROM), RUN (including
`<<EOF` / `<<-EOF` heredocs, `#!` shebangs, inline shell heredocs, and
`--mount=type=cache|secret|bind|tmpfs`), COPY and ADD (local files, tarballs,
`<<EOF` heredocs, `ADD <http(s)://url>` with `--checksum`, wildcards, `--from`
a stage or image, `--chown`, `--chmod`, hard links preserved as tar link
entries), CMD, ENTRYPOINT, SHELL, ENV, ARG, WORKDIR, USER, EXPOSE, LABEL,
VOLUME, STOPSIGNAL and `.dockerignore`. Not yet: ONBUILD and Git build URLs.
Each step's layer is cached by the step and the files it copies, so a rebuild
with nothing changed takes no time and gives the same image; steps that miss
run in a builder VM (4 vCPU, 4 GiB) with the context shared read-only, and the
agent writes each step's changes as a layer.

Inside a running container:

```bash
cargo run -p grav -- stats web        # CPU, memory, network (sampled every 2 s, 5 min kept)
cargo run -p grav -- top web          # its processes
cargo run -p grav -- ls web /etc      # list a folder (volumes included)
cargo run -p grav -- cat web /etc/os-release
cargo run -p grav -- cp ./cfg web:/etc/app      # copy local file or folder in
cargo run -p grav -- cp web:/etc/app ./cfg-out  # copy container file or folder out (`-` for stdout tar)
cargo run -p grav -- inspect web      # how it was configured, as JSON
```

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
4. ~~**Remote hosts**~~ (done): `gravitond --remote` serves TLS with a
   self-signed certificate and Bonjour; `grav pair` codes and links; the iPhone
   app discovers, pairs, pins the fingerprint and switches between Macs.
5. **Polish**: framed terminals with resize and exit codes, Stats, Files,
   Inspect and Terminal tabs, and a shell on the iPhone (done; terminals are
   drawn by [SwiftTerm](https://github.com/migueldeicaza/SwiftTerm), MIT);
   image builds with `grav build` and the Mac app's Build sheet (done);
   Developer ID signing and notarisation with `scripts/release.sh` (ready;
   needs your certificate); other Macs as hosts and a container network
   (done); the iPhone's Run, Pull, Stats, Files, Inspect, Activity and push
   alerts (done); app sandbox + XPC for the App Store.
