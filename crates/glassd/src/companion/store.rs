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

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::identity::Identity;
use super::{CompanionError, now_ms};

/// File name inside the state directory.
pub const FILE_NAME: &str = "companion.json";

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
    /// does not exist.
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Store, CompanionError> {
        let path = path.as_ref().to_path_buf();
        if path.exists() {
            let data = std::fs::read_to_string(&path)?;
            let sf: StateFile = serde_json::from_str(&data)
                .map_err(|e| CompanionError::Store(format!("parse {}: {e}", path.display())))?;
            let identity = Identity::from_secret_b64(&sf.identity_secret)?;
            Ok(Store {
                path,
                identity,
                paired: sf.paired_peer,
            })
        } else {
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
        // Write to a temp file then rename, so a crash never truncates the identity.
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
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
