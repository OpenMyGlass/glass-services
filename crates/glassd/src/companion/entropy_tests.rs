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
    let (events, cleared) = c.unpair();
    assert!(events.is_empty());
    assert!(cleared.is_ok());
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
    // A dial would run the handshake on the daemon's thread: it waits for the pool.
    assert!(c.connect_tcp(&addr).unwrap().is_empty());
    assert!(c.conn_fds().is_empty());
    pool.seed();
    c.on_rng_event();
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

#[test]
fn a_first_identity_waits_for_more_entropy_than_the_poll_says() {
    let d = tempfile::tempdir().unwrap();
    let mut pool = FakePool::new(d.path());
    let state = d.path().join("state");
    let level = d.path().join("entropy_avail");
    let mut c = glass(&state, &pool.path);
    c.entropy_avail = level.clone();
    std::fs::write(&level, "64\n").unwrap();

    pool.seed();
    c.on_rng_event();
    assert!(!state_file(&state).exists(), "a key from a thin pool");
    assert!(c.listener_fds().is_empty());
    // /dev/random stays readable, so the daemon must not poll it now.
    assert!(c.rng_fd().is_none());
    assert!(c.rng_next_deadline().is_some());

    // Not yet due, and then due with a level still too low.
    assert!(c.rng_tick().is_empty());
    assert!(!state_file(&state).exists());
    c.rng = super::Rng::Gathering { next_check_ms: 0 };
    std::fs::write(&level, "255\n").unwrap();
    c.rng_tick();
    assert!(!state_file(&state).exists());

    c.rng = super::Rng::Gathering { next_check_ms: 0 };
    std::fs::write(&level, "256\n").unwrap();
    c.rng_tick();
    assert!(state_file(&state).exists());
    assert_eq!(c.listener_fds().len(), 1);
    assert_eq!(c.rng_next_deadline(), None);
}

#[test]
fn a_stored_identity_does_not_wait_for_more_entropy() {
    let d = tempfile::tempdir().unwrap();
    let mut pool = FakePool::new(d.path());
    let state = d.path().join("state");
    drop(Store::create(Store::path_in(&state)).unwrap());
    let level = d.path().join("entropy_avail");
    std::fs::write(&level, "64\n").unwrap();
    let mut c = glass(&state, &pool.path);
    c.entropy_avail = level;
    pool.seed();
    c.on_rng_event();
    assert_eq!(c.listener_fds().len(), 1);
}

#[test]
fn a_dial_waits_through_the_gathering_too() {
    let d = tempfile::tempdir().unwrap();
    let mut pool = FakePool::new(d.path());
    let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = peer.local_addr().unwrap().to_string();
    let level = d.path().join("entropy_avail");
    std::fs::write(&level, "64\n").unwrap();
    let mut c = glass(&d.path().join("state"), &pool.path);
    c.entropy_avail = level.clone();
    pool.seed();
    c.on_rng_event();
    assert!(c.connect_tcp(&addr).unwrap().is_empty());
    assert!(c.conn_fds().is_empty());
    std::fs::write(&level, "300\n").unwrap();
    c.rng = super::Rng::Gathering { next_check_ms: 0 };
    c.rng_tick();
    assert_eq!(c.conn_fds().len(), 1);
    drop::<(TcpStream, _)>(peer.accept().unwrap());
}

#[test]
fn a_seed_makes_a_first_identity_not_wait_for_the_level() {
    let d = tempfile::tempdir().unwrap();
    let mut pool = FakePool::new(d.path());
    let state = d.path().join("state");
    let level = d.path().join("entropy_avail");
    std::fs::write(&level, "64\n").unwrap();
    let mut c = glass(&state, &pool.path);
    c.entropy_avail = level;
    c.seed_used = true;
    pool.seed();
    c.on_rng_event();
    assert!(state_file(&state).exists());
    assert_eq!(c.listener_fds().len(), 1);
    // The next boot has a seed of its own.
    assert!(state.join(super::entropy::SEED_FILE).exists());
}

#[test]
fn no_seed_is_saved_from_a_pool_that_passed_no_test() {
    let d = tempfile::tempdir().unwrap();
    let mut pool = FakePool::new(d.path());
    let state = d.path().join("state");
    drop(Store::create(Store::path_in(&state)).unwrap());
    let mut c = glass(&state, &pool.path);
    pool.seed();
    c.on_rng_event();
    assert_eq!(c.listener_fds().len(), 1);
    assert!(!state.join(super::entropy::SEED_FILE).exists());
}

#[test]
fn tcp_can_be_closed_for_a_while_with_its_connections_and_opened_again() {
    let d = tempfile::tempdir().unwrap();
    let mut pool = FakePool::new(d.path());
    let state = d.path().join("state");
    let mut c = glass(&state, &pool.path);

    // Closed before the pool opens the listeners: it stays closed when it does.
    assert!(c.set_tcp_closed(true).is_empty());
    pool.seed();
    c.on_rng_event();
    assert!(c.rng_ready());
    assert!(c.listener_fds().is_empty());

    assert!(c.set_tcp_closed(false).is_empty());
    assert_eq!(c.listener_fds().len(), 1);
    let port = c.listeners.tcp_port().unwrap();
    let peer = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let (_, transport) = c.listener_fds()[0];
    for _ in 0..200 {
        c.accept(transport);
        if !c.conn_fds().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(c.conn_fds().len(), 1);

    // Nothing is reachable on the access point.
    c.set_tcp_closed(true);
    assert!(c.conn_fds().is_empty(), "the connection went too");
    assert!(c.listener_fds().is_empty());
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    drop(peer);

    c.set_tcp_closed(false);
    assert_eq!(c.listener_fds().len(), 1);
}

/// The refusal that docs/api.md promises, word for word: the launcher shows "the phone link is
/// disabled" for it, and "a phone is already paired" for the other UNAVAILABLE.
#[test]
fn a_link_that_cannot_come_up_refuses_the_pairing_mode_in_the_documented_words() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let mut pool = FakePool::new(d.path());
    pool.seed();
    let state = d.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    // No identity can be stored, so the link stays down.
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o500)).unwrap();
    if std::fs::write(state.join("probe"), b"").is_ok() {
        eprintln!("skipped: running as root, the directory stays writable");
        return;
    }
    let mut c = glass(&state, &pool.path);
    assert!(!state_file(&state).exists());
    assert!(c.listener_fds().is_empty());
    assert_eq!(super::LINK_DISABLED, "the companion link is disabled");
    assert_eq!(
        c.open_pairing_window(0).err().as_deref(),
        Some(super::LINK_DISABLED)
    );
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
}
