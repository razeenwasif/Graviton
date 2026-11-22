//! Linux capability names, Docker default bitmask, and `--cap-add` / `--cap-drop` resolution.

/// Docker's default 14 container capabilities (linux/include/uapi/linux/capability.h bits).
pub const DEFAULT_CAP_MASK: u64 = (1 << 0) // CAP_CHOWN
    | (1 << 1) // CAP_DAC_OVERRIDE
    | (1 << 3) // CAP_FOWNER
    | (1 << 4) // CAP_FSETID
    | (1 << 5) // CAP_KILL
    | (1 << 6) // CAP_SETGID
    | (1 << 7) // CAP_SETUID
    | (1 << 8) // CAP_SETPCAP
    | (1 << 10) // CAP_NET_BIND_SERVICE
    | (1 << 13) // CAP_NET_RAW
    | (1 << 18) // CAP_SYS_CHROOT
    | (1 << 27) // CAP_MKNOD
    | (1 << 29) // CAP_AUDIT_WRITE
    | (1 << 31); // CAP_SETFCAP

/// All Linux capabilities through `CAP_CHECKPOINT_RESTORE` (bit 40).
pub const ALL_CAP_MASK: u64 = (1u64 << 41) - 1;

const CAPS: &[(&str, u32)] = &[
    ("CHOWN", 0),
    ("DAC_OVERRIDE", 1),
    ("DAC_READ_SEARCH", 2),
    ("FOWNER", 3),
    ("FSETID", 4),
    ("KILL", 5),
    ("SETGID", 6),
    ("SETUID", 7),
    ("SETPCAP", 8),
    ("LINUX_IMMUTABLE", 9),
    ("NET_BIND_SERVICE", 10),
    ("NET_BROADCAST", 11),
    ("NET_ADMIN", 12),
    ("NET_RAW", 13),
    ("IPC_LOCK", 14),
    ("IPC_OWNER", 15),
    ("SYS_MODULE", 16),
    ("SYS_RAWIO", 17),
    ("SYS_CHROOT", 18),
    ("SYS_PTRACE", 19),
    ("SYS_PACCT", 20),
    ("SYS_ADMIN", 21),
    ("SYS_BOOT", 22),
    ("SYS_NICE", 23),
    ("SYS_RESOURCE", 24),
    ("SYS_TIME", 25),
    ("SYS_TTY_CONFIG", 26),
    ("MKNOD", 27),
    ("LEASE", 28),
    ("AUDIT_WRITE", 29),
    ("AUDIT_CONTROL", 30),
    ("SETFCAP", 31),
    ("MAC_OVERRIDE", 32),
    ("MAC_ADMIN", 33),
    ("SYSLOG", 34),
    ("WAKE_ALARM", 35),
    ("BLOCK_SUSPEND", 36),
    ("AUDIT_READ", 37),
    ("PERFMON", 38),
    ("BPF", 39),
    ("CHECKPOINT_RESTORE", 40),
];

/// Validate and normalize a Linux capability name (`cap_net_bind_service` / `CAP_NET_BIND_SERVICE` / `ALL`).
/// Returns canonical uppercase name without the `CAP_` prefix (or `"ALL"`).
pub fn normalize_cap(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().to_ascii_uppercase();
    if trimmed.is_empty() {
        return Err("Linux capability name cannot be empty".into());
    }
    if trimmed == "ALL" {
        return Ok("ALL".into());
    }
    let name = trimmed.strip_prefix("CAP_").unwrap_or(&trimmed);
    if cap_bit(name).is_some() {
        Ok(name.to_string())
    } else {
        Err(format!("unknown Linux capability '{raw}'"))
    }
}

/// Look up the Linux capability bit number (`0..=40`) for a canonical name (without `CAP_`).
pub fn cap_bit(canonical: &str) -> Option<u32> {
    CAPS.iter().find(|&&(name, _)| name == canonical).map(|&(_, bit)| bit)
}

/// Resolve Docker-compatible effective capability bitmask from `--cap-add` and `--cap-drop`:
/// start with [`DEFAULT_CAP_MASK`], apply `cap_drop`, then apply `cap_add` (`cap_add` wins).
pub fn resolve_cap_mask(cap_add: &[String], cap_drop: &[String]) -> Result<u64, String> {
    let mut mask = DEFAULT_CAP_MASK;
    for raw in cap_drop {
        let cap = normalize_cap(raw)?;
        if cap == "ALL" {
            mask = 0;
        } else if let Some(bit) = cap_bit(&cap) {
            mask &= !(1u64 << bit);
        }
    }
    for raw in cap_add {
        let cap = normalize_cap(raw)?;
        if cap == "ALL" {
            mask = ALL_CAP_MASK;
        } else if let Some(bit) = cap_bit(&cap) {
            mask |= 1u64 << bit;
        }
    }
    Ok(mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mask_matches_docker_default_14() {
        assert_eq!(DEFAULT_CAP_MASK, 0x0000_0000_a804_25fb);
        assert_eq!(resolve_cap_mask(&[], &[]).unwrap(), 0x0000_0000_a804_25fb);
    }

    #[test]
    fn cap_add_after_cap_drop_ordering_and_normalization() {
        assert_eq!(resolve_cap_mask(&[], &["all".into()]).unwrap(), 0);
        assert_eq!(
            resolve_cap_mask(&["cap_net_bind_service".into()], &["ALL".into()]).unwrap(),
            1u64 << 10
        );
        assert_eq!(
            resolve_cap_mask(&["NET_ADMIN".into()], &[]).unwrap(),
            DEFAULT_CAP_MASK | (1u64 << 12)
        );
        assert_eq!(
            resolve_cap_mask(&["ALL".into()], &["ALL".into()]).unwrap(),
            ALL_CAP_MASK
        );
        assert_eq!(normalize_cap(" cap_sys_admin ").unwrap(), "SYS_ADMIN");
        assert!(normalize_cap("not_a_real_cap").is_err());
        assert!(normalize_cap("").is_err());
    }
}
