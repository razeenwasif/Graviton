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
  graviton-core    Image references, engine store, well-known paths
  graviton-image   Registry client, blob store, local image index, Dockerfile parser
  graviton-vmm     VM config + Hypervisor trait; Virtualization.framework backend
  graviton-agent   PID 1 inside each guest VM: unpacks images, runs containers, writes build layers
  gravitond        Engine daemon: control API on a Unix socket, one VM per container
  grav             CLI client
apps/
  project.yml      XcodeGen spec → Graviton.xcodeproj (generated, not committed)
  GravitonKit/     Shared Swift package: models, socket client, demo engine, theme, components
  Shared/          Swift shared by both apps: shell sessions drawn by SwiftTerm
  Mac/             macOS app (Containers, Run sheet, Logs, Terminal, Stats, Files, Inspect, Images + Pull sheet, menu bar item)
  iOS/             iOS companion (Containers, Detail, Logs, Shell, Stats, Files, Inspect, Run + Pull sheets, Activity, Hosts)
scripts/
  build-guest.sh   Guest kernel (Alpine linux-virt) + initramfs with the agent as /init
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
switch which Mac the window shows; it can pair too.

`grav service install|uninstall|status` runs the gravitond inside Graviton.app
at login on any Mac, without the source.

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

A terminal (`exec` with `"tty": true`) answers `attached`, then the connection
carries binary frames both ways: a kind byte (0 data, 1 resize, 2 end of input,
3 exit code), a big-endian u32 length and the payload. gravitond passes them
between the client and the guest agent untouched, over the Unix socket or TLS.

Build images from Dockerfiles:

```bash
cargo run -p grav -- build -t myapp .              # ./Dockerfile; the image is myapp:latest
cargo run -p grav -- build -f docker/Dockerfile --build-arg PY=3.12 --target deps .
cargo run -p grav -- run myapp
```

Supported: FROM (multi-stage, `AS name`, ARGs in FROM), RUN, COPY and ADD
(local files and tarballs, wildcards, `--from` a stage or image, `--chown`),
CMD, ENTRYPOINT, SHELL, ENV, ARG, WORKDIR, USER, EXPOSE, LABEL, VOLUME,
STOPSIGNAL and `.dockerignore`. `RUN --mount=type=cache` runs without the
cache. Not yet: heredocs, ONBUILD, `ADD <url>`, `--chmod` and secret or bind
mounts. Each step's layer is cached by the step and the files it copies, so a
rebuild with nothing changed takes no time and gives the same image; steps
that miss run in a builder VM (4 vCPU, 4 GiB) with the context shared
read-only, and the agent writes each step's changes as a layer.

Inside a running container:

```bash
cargo run -p grav -- stats web        # CPU, memory, network (sampled every 2 s, 5 min kept)
cargo run -p grav -- top web          # its processes
cargo run -p grav -- ls web /etc      # list a folder (volumes included)
cargo run -p grav -- cat web /etc/os-release
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
