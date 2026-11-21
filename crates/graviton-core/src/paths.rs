//! Well-known filesystem locations.
//!
//! The SwiftUI app resolves the same socket path (see `GravitonKit/Paths.swift`),
//! so keep the two in sync.

use std::path::PathBuf;

/// Environment variable that overrides the control socket location.
pub const SOCKET_ENV: &str = "GRAVITON_SOCKET";

/// Environment variable that overrides the Graviton support directory.
pub const DATA_DIR_ENV: &str = "GRAVITON_DATA_DIR";

/// `$GRAVITON_DATA_DIR`, else `~/Library/Application Support/Graviton`.
pub fn support_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV) {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join("Library/Application Support/Graviton")
}

/// Control socket: `$GRAVITON_SOCKET`, else `<support_dir>/gravitond.sock`.
pub fn socket_path() -> PathBuf {
    std::env::var_os(SOCKET_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| support_dir().join("gravitond.sock"))
}

/// Content-addressed blob store for image layers.
pub fn blobs_dir() -> PathBuf {
    support_dir().join("blobs")
}

/// Per-container state (rootfs, config, logs).
pub fn containers_dir() -> PathBuf {
    support_dir().join("containers")
}

/// Guest kernel and initramfs. `$GRAVITON_GUEST_DIR` wins; then the copy in
/// Graviton.app when gravitond runs from the app (`Contents/MacOS/gravitond`
/// next to `Contents/Resources/guest`), so it always matches its agent; then
/// the one `scripts/build-guest.sh` writes.
pub fn guest_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("GRAVITON_GUEST_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(bundled) = std::env::current_exe().ok().and_then(|exe| bundled_guest(&exe)) {
        return bundled;
    }
    support_dir().join("guest")
}

fn bundled_guest(exe: &std::path::Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    if macos.file_name()? != "MacOS" {
        return None;
    }
    let guest = macos.parent()?.join("Resources/guest");
    guest.join("vmlinux").is_file().then_some(guest)
}

/// Per-VM runtime files (console logs).
pub fn vms_dir() -> PathBuf {
    support_dir().join("vms")
}

/// Named volumes shared into containers (`-v name:/path`).
pub fn volumes_dir() -> PathBuf {
    support_dir().join("volumes")
}

/// Cached VM memory snapshots (`.vzsave` + per-slot machine identities).
pub fn snapshots_dir() -> PathBuf {
    support_dir().join("snapshots")
}

#[cfg(test)]
mod bundle_tests {
    use super::*;

    #[test]
    fn finds_the_guest_inside_an_app() {
        let app = std::env::temp_dir().join(format!("graviton-paths-{}/Graviton.app/Contents", std::process::id()));
        std::fs::create_dir_all(app.join("Resources/guest")).unwrap();
        std::fs::create_dir_all(app.join("MacOS")).unwrap();
        let exe = app.join("MacOS/gravitond");
        assert_eq!(bundled_guest(&exe), None); // no kernel yet
        std::fs::write(app.join("Resources/guest/vmlinux"), b"k").unwrap();
        assert_eq!(bundled_guest(&exe), Some(app.join("Resources/guest")));
        assert_eq!(bundled_guest(std::path::Path::new("/usr/local/bin/gravitond")), None);
        let _ = std::fs::remove_dir_all(app.parent().unwrap().parent().unwrap());
    }
}
