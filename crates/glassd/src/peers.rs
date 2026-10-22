//! Who may connect (docs/api.md, "Who may connect").
//!
//! On Android the socket is `0666` (no group of ours exists), so `glassd` checks each peer's
//! uid (`SO_PEERCRED`): root and system always, the uids of the allowed packages from
//! `/data/system/packages.list`, and explicitly allowed uids. Elsewhere the socket
//! permissions decide and the check is off unless something is allowed explicitly.

use std::path::{Path, PathBuf};

/// Android's `packages.list`.
pub const PACKAGES_LIST: &str = "/data/system/packages.list";

/// Always allowed: root and Android's system uid.
pub const ALWAYS: [u32; 2] = [0, 1000];

/// The peer check.
#[derive(Debug, Clone)]
pub struct PeerPolicy {
    enabled: bool,
    uids: Vec<u32>,
    packages: Vec<String>,
    packages_list: PathBuf,
    /// uids of `packages` found in the list when it was last read.
    package_uids: Vec<u32>,
    /// The list's modification time when it was last read.
    read_mtime: Option<std::time::SystemTime>,
}

/// The uids of `names` in the text of a `packages.list` (`name uid debuggable dir seinfo
/// gids` per line; only the first two fields are used).
pub fn package_uids(text: &str, names: &[String]) -> Vec<u32> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let (Some(name), Some(uid)) = (f.next(), f.next()) else {
            continue;
        };
        if names.iter().any(|n| n == name)
            && let Ok(uid) = uid.parse::<u32>()
            && !out.contains(&uid)
        {
            out.push(uid);
        }
    }
    out
}

impl PeerPolicy {
    /// `enabled`: check peers at all. With it off every peer is allowed.
    pub fn new(enabled: bool, uids: Vec<u32>, packages: Vec<String>, list: &Path) -> PeerPolicy {
        PeerPolicy {
            enabled,
            uids,
            packages,
            packages_list: list.to_path_buf(),
            package_uids: Vec::new(),
            read_mtime: None,
        }
    }

    /// No check.
    pub fn open() -> PeerPolicy {
        PeerPolicy::new(false, Vec::new(), Vec::new(), Path::new(PACKAGES_LIST))
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Whether a peer with `uid` may connect. Reads the package list again when the uid is
    /// not known and the list changed since it was last read (an app installed or updated).
    pub fn allows(&mut self, uid: u32) -> bool {
        if !self.enabled || ALWAYS.contains(&uid) || self.uids.contains(&uid) {
            return true;
        }
        if self.package_uids.contains(&uid) {
            return true;
        }
        if self.packages.is_empty() {
            return false;
        }
        let mtime = std::fs::metadata(&self.packages_list)
            .and_then(|m| m.modified())
            .ok();
        if mtime.is_none() || mtime != self.read_mtime {
            match std::fs::read_to_string(&self.packages_list) {
                Ok(text) => {
                    self.package_uids = package_uids(&text, &self.packages);
                    self.read_mtime = mtime;
                }
                Err(e) => {
                    crate::warn!("cannot read {}: {e}", self.packages_list.display());
                    self.package_uids.clear();
                    self.read_mtime = None;
                }
            }
        }
        self.package_uids.contains(&uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "\
org.openmyglass.launcher 10012 0 /data/data/org.openmyglass.launcher platform 3002,3003
com.example.other 10013 0 /data/data/com.example.other default none
org.openmyglass.glass 10020 1 /data/data/org.openmyglass.glass default 3002
broken-line
";

    #[test]
    fn parses_packages_list() {
        let names = vec![
            "org.openmyglass.launcher".to_string(),
            "org.openmyglass.glass".to_string(),
            "missing".to_string(),
        ];
        assert_eq!(package_uids(LIST, &names), vec![10012, 10020]);
        assert!(package_uids(LIST, &[]).is_empty());
    }

    #[test]
    fn policy() {
        let dir = std::env::temp_dir().join(format!("glassd-peers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let list = dir.join("packages.list");
        std::fs::write(&list, LIST).unwrap();
        let mut p = PeerPolicy::new(
            true,
            vec![2000],
            vec!["org.openmyglass.launcher".into()],
            &list,
        );
        assert!(p.allows(0) && p.allows(1000) && p.allows(2000));
        assert!(p.allows(10012));
        assert!(!p.allows(10013) && !p.allows(10020));
        // A newly installed package appears after the list changes.
        std::fs::write(
            &list,
            format!("{LIST}org.openmyglass.launcher 10044 0 /x platform none\n"),
        )
        .unwrap();
        // Make sure the modification time differs on coarse file systems.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&list)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert!(p.allows(10044));
        // Off: everybody.
        assert!(PeerPolicy::open().allows(12345));
        // A missing list allows only the fixed uids.
        let mut q = PeerPolicy::new(true, vec![], vec!["a".into()], &dir.join("none"));
        assert!(!q.allows(10012) && q.allows(1000));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
