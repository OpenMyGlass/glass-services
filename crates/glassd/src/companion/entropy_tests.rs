//! The link waits for the random pool without holding anything else up, and generates no key
//! before it is seeded.

use std::net::TcpStream;
use std::path::Path;

use super::entropy::testing::FakePool;
use super::link::DeviceState;
use super::store::{self, Store};
use super::{Companion, state_file};

/// Port 0: the OS picks one, so that the tests can run side by side.
fn glass(dir: &Path, rng: &Path) -> Companion {
    Companion::with_rng_device(dir, DeviceState::default(), Some(0), false, rng).unwrap()
}

#[test]
fn no_identity_and_no_listener_before_the_pool_is_seeded() {
    let d = tempfile::tempdir().unwrap();
    let mut pool = FakePool::new(d.path());
    let state = d.path().join("state");
    let mut c = glass(&state, &pool.path);

    // Nothing that would have used the pool has happened, and what needs none is there.
    assert!(c.rng_fd().is_some());
    assert!(!state_file(&state).exists(), "a key from an unseeded pool");
    assert!(c.listener_fds().is_empty());
    assert!(!c.paired());
    assert!(c.notes_available());

    // An event that was not the pool changes nothing.
    c.on_rng_event();
    assert!(c.rng_fd().is_some());
    assert!(!state_file(&state).exists());

    // Forgetting a bond that cannot exist yet is harmless.
    assert!(c.unpair().is_empty());
    // A dial would run the handshake on the daemon's thread: refused, without dialing.
    assert!(c.connect_tcp("192.0.2.1:7").is_err());

    pool.seed();
    c.on_rng_event();
    assert!(c.rng_fd().is_none());
    assert!(state_file(&state).exists());
    assert_eq!(c.listener_fds().len(), 1);
}

#[test]
fn a_stored_identity_is_kept_but_the_listeners_still_wait() {
    let d = tempfile::tempdir().unwrap();
    let mut pool = FakePool::new(d.path());
    let state = d.path().join("state");
    let mut stored = Store::create(Store::path_in(&state)).unwrap();
    let identity = stored.identity().public_b64();
    stored
        .set_paired(store::record("peer".into(), "Phone".into()))
        .unwrap();

    let mut c = glass(&state, &pool.path);
    // The bond is known at once. The handshake needs a nonce, so nobody is accepted yet.
    assert!(c.paired());
    assert_eq!(c.paired_name(), Some("Phone"));
    assert!(c.rng_fd().is_some());
    assert!(c.listener_fds().is_empty());

    pool.seed();
    c.on_rng_event();
    assert_eq!(c.listener_fds().len(), 1);
    assert!(c.paired());
    let again = Store::load(Store::path_in(&state)).unwrap().unwrap();
    assert_eq!(again.identity().public_b64(), identity);
}

#[test]
fn a_seeded_pool_changes_nothing_at_start() {
    let d = tempfile::tempdir().unwrap();
    let mut pool = FakePool::new(d.path());
    pool.seed();
    let state = d.path().join("state");
    let c = glass(&state, &pool.path);
    assert!(c.rng_fd().is_none());
    assert!(state_file(&state).exists());
    assert_eq!(c.listener_fds().len(), 1);
}

#[test]
fn a_connection_runs_once_the_link_is_up() {
    let d = tempfile::tempdir().unwrap();
    let mut pool = FakePool::new(d.path());
    let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = peer.local_addr().unwrap().to_string();
    let mut c = glass(&d.path().join("state"), &pool.path);
    assert!(c.connect_tcp(&addr).is_err());
    pool.seed();
    c.on_rng_event();
    c.connect_tcp(&addr).unwrap();
    assert_eq!(c.conn_fds().len(), 1);
    drop::<(TcpStream, _)>(peer.accept().unwrap());
}

#[test]
fn a_corrupt_state_file_disables_the_link_at_start_as_before() {
    let d = tempfile::tempdir().unwrap();
    let pool = FakePool::new(d.path());
    let state = d.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state_file(&state), "not json").unwrap();
    assert!(
        Companion::with_rng_device(&state, DeviceState::default(), None, false, &pool.path)
            .is_err()
    );
}

#[test]
fn no_random_device_on_a_kernel_with_the_syscall_still_comes_up() {
    let d = tempfile::tempdir().unwrap();
    let state = d.path().join("state");
    let c = glass(&state, &d.path().join("no-such-device"));
    assert!(c.rng_fd().is_none());
    assert_eq!(c.listener_fds().len(), 1);
}
