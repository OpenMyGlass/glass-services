//! Persistent companion state: the Glass's identity and the single bonded companion.
//!
//! This is what makes the pairing **persistent and one-time**: once a companion is bonded it
//! is remembered across reboots, re-used on every reconnect, and any other peer is rejected
//! until [`Store::clear_paired`] (unpair). On Model B this file must live on persistent
//! storage (`/var/lib/glassd`), or the Glass forgets its companion on reboot; the daemon
//! passes its `--state-dir` (`/var/lib/glassd` by default; `/data/glassd` on Android).
//!
//! Mirrors `omg-core`'s `store` module (atomic temp-write + rename), so both ends persist the
//! same `PairingRecord` shape.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::identity::Identity;
use super::{CompanionError, now_ms};
use crate::warn;

/// File name inside the state directory.
pub const FILE_NAME: &str = "companion.json";
/// Owner read and write only: the file holds the identity secret.
const SECRET_MODE: u32 = 0o600;

/// The bonded companion. Its public key is the trust anchor checked on every reconnect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingRecord {
    /// Base64 of the companion's ed25519 public key.
    pub peer_pub: String,
    /// Friendly name captured at pairing.
    pub peer_name: String,
    /// When the bond was established, Unix ms.
    pub bonded_at_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StateFile {
    identity_secret: String,
    paired_peer: Option<PairingRecord>,
}

/// On-disk companion state with an in-memory view.
pub struct Store {
    path: PathBuf,
    identity: Identity,
    paired: Option<PairingRecord>,
}

impl Store {
    /// Loads the store from `path`, or creates a fresh identity and persists it if the file
    /// does not exist. Creating blocks on an unseeded random pool: see [`Store::create`].
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Store, CompanionError> {
        match Store::load(path.as_ref())? {
            Some(store) => Ok(store),
            None => Store::create(path),
        }
    }

    /// Loads the store from `path`; `None` if the file does not exist. Needs no randomness.
    pub fn load(path: impl AsRef<Path>) -> Result<Option<Store>, CompanionError> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            return Ok(None);
        }
        tighten_older_files(&path);
        let data = std::fs::read_to_string(&path)?;
        let sf: StateFile = serde_json::from_str(&data)
            .map_err(|e| CompanionError::Store(format!("parse {}: {e}", path.display())))?;
        let identity = Identity::from_secret_b64(&sf.identity_secret)?;
        Ok(Some(Store {
            path,
            identity,
            paired: sf.paired_peer,
        }))
    }

    /// Creates a fresh identity and persists it at `path`. This generates the long-term secret,
    /// so it blocks until the random pool is seeded: on the daemon's thread, call it only once
    /// [`super::entropy::PoolWait`] says so.
    pub fn create(path: impl AsRef<Path>) -> Result<Store, CompanionError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let store = Store {
            path,
            identity: Identity::generate(),
            paired: None,
        };
        store.save()?;
        Ok(store)
    }

    /// The directory file name used inside a state directory.
    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join(FILE_NAME)
    }

    /// Our long-term identity.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// The bonded companion, if any.
    pub fn paired(&self) -> Option<&PairingRecord> {
        self.paired.as_ref()
    }

    /// Records a newly bonded companion and persists.
    pub fn set_paired(&mut self, record: PairingRecord) -> Result<(), CompanionError> {
        self.paired = Some(record);
        self.save()
    }

    /// Forgets the bonded companion (unpair) and persists.
    pub fn clear_paired(&mut self) -> Result<(), CompanionError> {
        self.paired = None;
        self.save()
    }

    fn save(&self) -> Result<(), CompanionError> {
        let sf = StateFile {
            identity_secret: self.identity.secret_b64(),
            paired_peer: self.paired.clone(),
        };
        let json = serde_json::to_string_pretty(&sf)
            .map_err(|e| CompanionError::Store(format!("serialize: {e}")))?;
        // Write to a temp file then rename, so a crash never truncates the identity, and sync,
        // so neither does losing power. A leftover temp file would keep its old mode.
        let tmp = self.path.with_extension("tmp");
        remove_if_there(&tmp)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(SECRET_MODE)
            .open(&tmp)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &self.path)?;
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }
}

fn remove_if_there(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Older versions left the state file readable by everyone, and a crash could leave its temp
/// file. Failing here is no reason to lose the link, since the file can still be read: a state
/// directory gone read-only would refuse this and nothing else.
fn tighten_older_files(path: &Path) {
    let mode = std::fs::Permissions::from_mode(SECRET_MODE);
    if let Err(e) = std::fs::set_permissions(path, mode) {
        warn!("companion: cannot restrict {} ({e})", path.display());
    }
    if let Err(e) = remove_if_there(&path.with_extension("tmp")) {
        warn!(
            "companion: cannot remove the temp file of {} ({e})",
            path.display()
        );
    }
}

/// Builds a [`PairingRecord`] for `peer_pub`/`peer_name` bonded now.
pub fn record(peer_pub: String, peer_name: String) -> PairingRecord {
    PairingRecord {
        peer_pub,
        peer_name,
        bonded_at_ms: now_ms(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("glassd-companion-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn the_secret_is_readable_only_by_the_owner() {
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let d = dir("mode");
        let path = d.join(FILE_NAME);
        let tmp = path.with_extension("tmp");
        drop(Store::load_or_create(&path).unwrap());
        assert_eq!(mode(&path), SECRET_MODE);
        assert!(!tmp.exists());
        // What an older version left: both are dealt with by a load, with no save needed.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(&tmp, "stale").unwrap();
        let mut s = Store::load(&path).unwrap().unwrap();
        assert_eq!(mode(&path), SECRET_MODE);
        assert!(!tmp.exists());
        s.clear_paired().unwrap();
        assert_eq!(mode(&path), SECRET_MODE);
    }

    #[test]
    fn persists_identity_and_pairing() {
        let d = dir("store");
        let path = d.join("sub").join(FILE_NAME);
        let pub_a;
        {
            let mut s = Store::load_or_create(&path).unwrap();
            pub_a = s.identity().public_b64();
            assert!(s.paired().is_none());
            s.set_paired(record("peerpub".into(), "Companion".into()))
                .unwrap();
        }
        {
            let s = Store::load_or_create(&path).unwrap();
            assert_eq!(s.identity().public_b64(), pub_a);
            assert_eq!(s.paired().unwrap().peer_pub, "peerpub");
            assert_eq!(s.paired().unwrap().peer_name, "Companion");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unpair_clears() {
        let d = dir("unpair");
        let path = d.join(FILE_NAME);
        let mut s = Store::load_or_create(&path).unwrap();
        s.set_paired(record("p".into(), "n".into())).unwrap();
        s.clear_paired().unwrap();
        let s2 = Store::load_or_create(&path).unwrap();
        assert!(s2.paired().is_none());
        let _ = std::fs::remove_dir_all(&d);
    }
}
