//! Well-known filesystem locations.
//!
//! The SwiftUI app resolves the same socket path (see `GravitonKit/Paths.swift`),
//! so keep the two in sync.

use std::path::PathBuf;

/// Environment variable that overrides the control socket location.
pub const SOCKET_ENV: &str = "GRAVITON_SOCKET";

/// `~/Library/Application Support/Graviton`
pub fn support_dir() -> PathBuf {
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
