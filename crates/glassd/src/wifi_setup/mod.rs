//! Wi-Fi setup without a companion: the Glass opens its own access point for a while, and a small
//! page takes the network's name and password. See docs/plans/2026-11-07-wifi-setup-ap-design.md.
//!
//! [`WifiSetup`] is a state machine for the daemon's `poll` loop, driven by a clock it is given,
//! so its tests run on the host with a recording [`Radio`]. It never blocks on the waits: firmware
//! settling, `hostapd` starting and the join all happen across ticks. Setting a password on the
//! Glass goes:
//!
//! ```text
//! Off -> Settling -> Starting -> Serving -> Flushing -> Leaving -> Joining -> Off (joined)
//!                        \            \___ timeout, cancel ___> Leaving -> Off        |
//!                         \___ no AP-ENABLED ________________> Leaving -> Off        |
//!                                                                 ^___ no lease, cancel
//! ```
//!
//! The password of the access point and the password that is typed are never logged: this module
//! logs neither, the access point's goes only to `WifiSetupState` while `Serving`, and the
//! network's only into a private file.

pub mod ap;
pub mod http;
pub mod portal;

use crate::radio::Radio;
use crate::{info, warn};
use ap::{ApError, ApPaths};
use glass_api::proto::{WifiSetupPhase, WifiSetupState};
use http::{Lang, Network};
use portal::{Binding, Portal};
use std::os::fd::RawFd;
use std::path::PathBuf;

/// `WifiSetupState.error`: the setting is off.
pub const ERR_DISABLED: &str = "disabled";
/// Airplane mode is on.
pub const ERR_AIRPLANE: &str = "airplane";
/// The random pool is not seeded yet.
pub const ERR_NOT_READY: &str = "not-ready";
pub const ERR_NO_TOOLS: &str = "no-tools";
pub const ERR_NO_FIRMWARE: &str = "no-firmware";
pub const ERR_FILES: &str = "files";
pub const ERR_LINK_UP: &str = "link-up";
pub const ERR_FIRMWARE_MODE: &str = "firmware-mode";
pub const ERR_SERVE: &str = "serve";
/// `hostapd` did not report the access point up, or it stopped.
pub const ERR_AP: &str = "ap";
/// Nobody sent a network in time.
pub const ERR_TIMEOUT: &str = "timeout";
/// The network was refused or had no lease in time; the previous network is kept.
pub const ERR_JOIN: &str = "join";

/// The file a network is tried from, in the state directory. Renamed to `wifi.conf` once it works.
const TRIED_CONF: &str = "wifi.conf.try";
/// Alphabet of the access point's password: no `0 o 1 l i`, which the prism's small text blurs.
const ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";
/// About 59 bits. See the design doc for why that is enough for a session.
const PASSWORD_LEN: usize = 12;
const _: () = assert!(PASSWORD_LEN >= 8, "WPA2 needs at least 8");
/// hostapd's entropy file: its counter (2 is "ready") and 20 bytes for its pool.
const ENTROPY_LEN: usize = 21;
const DAEMONS_MARGIN_S: u64 = 30;
/// How often the `hostapd` log and the join are checked.
const CHECK_US: u64 = 1_000_000;

/// How long each stage may take, in microseconds.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// Each wait around the firmware (the experiments on the Glass waited this long).
    pub settle_us: u64,
    /// For `hostapd` to report the access point up (30 s on the Glass; it took about 9 s).
    pub enable_us: u64,
    /// For the owner to send a network.
    pub session_us: u64,
    /// For the page's answer to get out before the access point goes.
    pub flush_us: u64,
    /// To get a lease from the new network.
    pub join_us: u64,
}

impl Timeouts {
    /// How long `hostapd` and `udhcpd` may run at all. This is what ends the access point when
    /// the daemon was killed and not restarted: a whole session, with a margin for the daemon
    /// to get there first.
    pub fn daemons_limit_s(&self) -> u64 {
        (self.enable_us + self.session_us + self.flush_us) / 1_000_000 + DAEMONS_MARGIN_S
    }
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            settle_us: 2_000_000,
            enable_us: 30_000_000,
            session_us: 600_000_000,
            flush_us: 1_500_000,
            join_us: 60_000_000,
        }
    }
}

/// How one [`WifiSetup`] is wired.
#[derive(Debug, Clone)]
pub struct Config {
    pub paths: ApPaths,
    pub binding: Binding,
    /// `wifi.conf` in the state directory.
    pub wifi_conf: PathBuf,
    pub timeouts: Timeouts,
    /// `--mock`: there is no `hostapd` to report, so the access point counts as up.
    pub assume_enabled: bool,
}

/// The conditions outside this module, read when a session is asked for.
#[derive(Debug, Clone, Copy)]
pub struct Context {
    /// `wifi_setup_ap_enabled`.
    pub enabled: bool,
    pub airplane: bool,
    /// The random pool may be used: see `Companion::rng_ready`.
    pub pool_ready: bool,
    pub lang: Lang,
}

/// The access point's name and password for one session. Its `Debug` does not show them.
#[derive(Clone)]
struct Credentials {
    ssid: String,
    password: String,
    token: String,
    entropy: [u8; ENTROPY_LEN],
    lang: Lang,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials(redacted)")
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A character of [`ALPHABET`] for each byte in `random` that takes one evenly: the others are
/// dropped, since a plain remainder would favour the first characters.
fn alphabet_chars(random: &[u8]) -> impl Iterator<Item = char> {
    let limit = 256 - 256 % ALPHABET.len();
    random
        .iter()
        .filter(move |&&b| usize::from(b) < limit)
        .map(|&b| char::from(ALPHABET[usize::from(b) % ALPHABET.len()]))
}

fn generate(lang: Lang) -> Result<Credentials, getrandom::Error> {
    let mut password = String::new();
    while password.len() < PASSWORD_LEN {
        let mut random = [0u8; 32];
        getrandom::getrandom(&mut random)?;
        password.extend(alphabet_chars(&random).take(PASSWORD_LEN - password.len()));
    }
    let mut suffix = [0u8; 2];
    getrandom::getrandom(&mut suffix)?;
    let mut token = [0u8; 16];
    getrandom::getrandom(&mut token)?;
    let mut entropy = [0u8; ENTROPY_LEN];
    getrandom::getrandom(&mut entropy)?;
    entropy[0] = 2;
    Ok(Credentials {
        ssid: format!("OMG-Glass-{}", hex(&suffix)),
        password,
        token: hex(&token),
        entropy,
        lang,
    })
}

/// What comes after the access point has gone.
#[derive(Debug)]
enum Then {
    Join(Network),
    /// Back to what the settings say, with this error if it is not `None`, about this network
    /// if there was one.
    Restore {
        error: Option<&'static str>,
        network: String,
    },
}

impl Then {
    fn restore(error: Option<&'static str>) -> Then {
        Then::Restore {
            error,
            network: String::new(),
        }
    }
}

#[derive(Debug)]
enum Phase {
    Off,
    /// Waiting on the firmware before the start. `downed`: the second of the two waits.
    Settling {
        until: u64,
        downed: bool,
        creds: Credentials,
    },
    /// `hostapd` is running, not up yet.
    Starting {
        until: u64,
        next_check: u64,
        creds: Credentials,
    },
    /// The access point and the page are up.
    Serving {
        until: u64,
        next_check: u64,
        creds: Credentials,
        portal: PortalSlot,
        clients: u32,
    },
    /// A form was accepted: its answer is going out.
    Flushing {
        until: u64,
        portal: PortalSlot,
        network: Network,
    },
    /// The access point is going. `downed`: the second of the two waits.
    Leaving {
        until: u64,
        downed: bool,
        then: Then,
    },
    /// Trying the new network.
    Joining {
        until: u64,
        next_check: u64,
        ssid: String,
    },
}

/// A portal in [`Phase`], which needs `Debug`.
struct PortalSlot(Portal);

impl std::fmt::Debug for PortalSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Portal")
    }
}

/// How a session ended, for the daemon to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finished {
    /// On the new network: `wifi_enabled` must now be on.
    Joined,
    /// Radio released: drive it to what the settings say.
    Restore,
}

/// What the daemon must do after a call.
#[derive(Debug, Default)]
pub struct Update {
    /// To publish on `TOPIC_WIFI_SETUP`.
    pub state: Option<WifiSetupState>,
    pub finished: Option<Finished>,
}

/// The session, if any.
pub struct WifiSetup {
    cfg: Config,
    phase: Phase,
    log: ap::LogReader,
}

fn state(phase: WifiSetupPhase) -> WifiSetupState {
    WifiSetupState {
        phase: phase as i32,
        ..Default::default()
    }
}

fn failed(error: &str) -> WifiSetupState {
    WifiSetupState {
        error: error.into(),
        ..state(WifiSetupPhase::Failed)
    }
}

fn seconds(until: u64, now_us: u64) -> u32 {
    (until.saturating_sub(now_us) / 1_000_000) as u32
}

impl WifiSetup {
    pub fn new(cfg: Config) -> WifiSetup {
        WifiSetup {
            cfg,
            phase: Phase::Off,
            log: ap::LogReader::default(),
        }
    }

    /// Whether a session holds the radio. While true the system must stay awake.
    pub fn active(&self) -> bool {
        !matches!(self.phase, Phase::Off)
    }

    fn tried_conf(&self) -> PathBuf {
        self.cfg.wifi_conf.with_file_name(TRIED_CONF)
    }

    /// Ends what a daemon that died in a session left: the daemons, or the supplicant and the DHCP
    /// client if it was already joining, with the same waits as any other end. `radio` knows
    /// nothing of those; the files tell. For the daemon's start.
    pub fn cleanup_stale(&mut self, radio: &mut Radio, now_us: u64) -> Update {
        let ap = self.cfg.paths.run_dir.exists();
        let joining = self.tried_conf().exists();
        if !ap && !joining {
            return Update::default();
        }
        warn!("wifi-setup: ending a session left by an earlier run");
        radio.hold_wifi();
        if joining {
            radio.stop_association();
            // It holds a password, and was never confirmed.
            let _ = std::fs::remove_file(self.tried_conf());
        }
        self.leave(radio, Then::restore(None), now_us)
    }

    /// The daemon is exiting: ends the session now, waiting here since there is no later.
    pub fn shutdown(&mut self, radio: &mut Radio) {
        if !self.active() {
            return;
        }
        info!("wifi-setup: ending the session for the exit");
        radio.ap_stop_daemons(&self.cfg.paths);
        if matches!(self.phase, Phase::Joining { .. }) || self.tried_conf().exists() {
            radio.stop_association();
            let _ = std::fs::remove_file(self.tried_conf());
        }
        std::thread::sleep(std::time::Duration::from_micros(
            self.cfg.timeouts.settle_us,
        ));
        radio.ap_teardown(&self.cfg.paths);
        radio.release_wifi();
        self.phase = Phase::Off;
    }

    /// The state to tell a client asking now.
    fn current(&self, now_us: u64) -> WifiSetupState {
        match &self.phase {
            Phase::Off => state(WifiSetupPhase::Off),
            Phase::Settling { .. } | Phase::Starting { .. } => state(WifiSetupPhase::Starting),
            Phase::Serving {
                until,
                creds,
                portal,
                clients,
                ..
            } => WifiSetupState {
                ssid: creds.ssid.clone(),
                password: creds.password.clone(),
                url: portal.0.url(),
                seconds_left: seconds(*until, now_us),
                clients: *clients,
                ..state(WifiSetupPhase::Serving)
            },
            Phase::Flushing { network, .. } => WifiSetupState {
                network: network.ssid.clone(),
                ..state(WifiSetupPhase::Joining)
            },
            Phase::Leaving { then, .. } => match then {
                Then::Join(network) => WifiSetupState {
                    network: network.ssid.clone(),
                    ..state(WifiSetupPhase::Joining)
                },
                Then::Restore { .. } => state(WifiSetupPhase::Stopping),
            },
            Phase::Joining { ssid, .. } => WifiSetupState {
                network: ssid.clone(),
                ..state(WifiSetupPhase::Joining)
            },
        }
    }

    fn publish_current(&self, now_us: u64) -> Update {
        Update {
            state: Some(self.current(now_us)),
            finished: None,
        }
    }

    /// `WifiSetupRequest{start: true}`. A session that is already running stays as it is.
    pub fn start(&mut self, radio: &mut Radio, ctx: Context, now_us: u64) -> Update {
        if self.active() {
            return self.publish_current(now_us);
        }
        let refuse = |error| Update {
            state: Some(failed(error)),
            finished: None,
        };
        if !ctx.enabled {
            return refuse(ERR_DISABLED);
        }
        if ctx.airplane {
            return refuse(ERR_AIRPLANE);
        }
        // Reading the crate's randomness before this would stop the whole daemon on 3.4.
        if !ctx.pool_ready {
            return refuse(ERR_NOT_READY);
        }
        if let Err(e) = radio.ap_preflight(&self.cfg.paths) {
            return refuse(e.code());
        }
        let creds = match generate(ctx.lang) {
            Ok(c) => c,
            Err(e) => {
                warn!("wifi-setup: no randomness ({e})");
                return refuse(ERR_NOT_READY);
            }
        };
        info!("wifi-setup: starting");
        self.log = ap::LogReader::default();
        radio.hold_wifi();
        radio.ap_prepare();
        self.phase = Phase::Settling {
            until: now_us + self.cfg.timeouts.settle_us,
            downed: false,
            creds,
        };
        self.publish_current(now_us)
    }

    /// `WifiSetupRequest{start: false}`, the setting going off, or airplane mode.
    pub fn stop(&mut self, radio: &mut Radio, now_us: u64) -> Update {
        match &mut self.phase {
            Phase::Off => return self.publish_current(now_us),
            // Already going. One that was to join afterwards must not: that would turn the radio
            // on for a minute and could replace the network, after a cancel or in airplane mode.
            Phase::Leaving { then, .. } => {
                if matches!(then, Then::Join(_)) {
                    info!("wifi-setup: the network will not be joined");
                    *then = Then::restore(None);
                }
                return self.publish_current(now_us);
            }
            Phase::Joining { .. } => {
                return self.abandon_join(radio, Then::restore(None), now_us);
            }
            _ => {}
        }
        self.leave(radio, Then::restore(None), now_us)
    }

    /// Stops trying the network, keeping the old one. The radio goes back after the wait for the
    /// supplicant to go.
    fn abandon_join(&mut self, radio: &mut Radio, then: Then, now_us: u64) -> Update {
        radio.abandon_join(self.cfg.wifi_conf.clone());
        let _ = std::fs::remove_file(self.tried_conf());
        self.phase = Phase::Leaving {
            until: now_us + self.cfg.timeouts.settle_us,
            downed: true,
            then,
        };
        self.publish_current(now_us)
    }

    fn leave(&mut self, radio: &mut Radio, then: Then, now_us: u64) -> Update {
        radio.ap_stop_daemons(&self.cfg.paths);
        self.phase = Phase::Leaving {
            until: now_us + self.cfg.timeouts.settle_us,
            downed: false,
            then,
        };
        self.publish_current(now_us)
    }

    fn fail_start(&mut self, radio: &mut Radio, e: ApError, now_us: u64) -> Update {
        if let ApError::WrongMode(mode) = e {
            warn!("wifi-setup: the firmware came up in mode {mode}, not AP");
        }
        self.leave(radio, Then::restore(Some(e.code())), now_us)
    }

    /// When [`WifiSetup::tick`] next has work (`CLOCK_MONOTONIC` µs).
    pub fn next_deadline(&self) -> Option<u64> {
        match &self.phase {
            Phase::Off => None,
            Phase::Settling { until, .. }
            | Phase::Flushing { until, .. }
            | Phase::Leaving { until, .. } => Some(*until),
            Phase::Starting {
                until, next_check, ..
            }
            | Phase::Serving {
                until, next_check, ..
            }
            | Phase::Joining {
                until, next_check, ..
            } => Some((*until).min(*next_check)),
        }
    }

    /// The page's descriptors to poll.
    pub fn fds(&self) -> Vec<(RawFd, libc::c_short)> {
        match &self.phase {
            Phase::Serving { portal, .. } | Phase::Flushing { portal, .. } => portal.0.fds(),
            _ => Vec::new(),
        }
    }

    /// A descriptor from [`WifiSetup::fds`] is ready.
    pub fn on_ready(&mut self, fd: RawFd, now_us: u64) -> Update {
        let accepted = match &mut self.phase {
            Phase::Serving { portal, .. } | Phase::Flushing { portal, .. } => {
                portal.0.on_ready(fd, now_us)
            }
            _ => None,
        };
        let Some(network) = accepted else {
            return Update::default();
        };
        // No password of any kind in the state from here on: the launcher takes it off the prism.
        let until = now_us + self.cfg.timeouts.flush_us;
        match std::mem::replace(&mut self.phase, Phase::Off) {
            Phase::Serving { portal, .. } => {
                info!("wifi-setup: a network was sent");
                self.phase = Phase::Flushing {
                    until,
                    portal,
                    network,
                };
                self.publish_current(now_us)
            }
            other => {
                self.phase = other;
                Update::default()
            }
        }
    }

    /// Moves on: to be called when [`WifiSetup::next_deadline`] has passed.
    pub fn tick(&mut self, radio: &mut Radio, now_us: u64) -> Update {
        if self.next_deadline().is_none_or(|d| now_us < d) {
            return Update::default();
        }
        match std::mem::replace(&mut self.phase, Phase::Off) {
            Phase::Off => Update::default(),
            Phase::Settling {
                downed: false,
                creds,
                ..
            } => {
                radio.ap_link_down();
                self.phase = Phase::Settling {
                    until: now_us + self.cfg.timeouts.settle_us,
                    downed: true,
                    creds,
                };
                Update::default()
            }
            Phase::Settling { creds, .. } => {
                let run = ap::ApCredentials {
                    ssid: &creds.ssid,
                    password: &creds.password,
                    entropy: &creds.entropy,
                };
                let limit = self.cfg.timeouts.daemons_limit_s();
                match radio.ap_start(&self.cfg.paths, run, limit, !self.cfg.assume_enabled) {
                    Ok(()) => {
                        self.phase = Phase::Starting {
                            until: now_us + self.cfg.timeouts.enable_us,
                            next_check: now_us + CHECK_US,
                            creds,
                        };
                        Update::default()
                    }
                    Err(e) => self.fail_start(radio, e, now_us),
                }
            }
            Phase::Starting {
                until,
                creds,
                next_check: _,
            } => self.check_starting(radio, until, creds, now_us),
            Phase::Serving {
                until,
                creds,
                portal,
                clients,
                next_check: _,
            } => self.check_serving(radio, until, creds, portal, clients, now_us),
            Phase::Flushing { network, .. } => self.leave(radio, Then::Join(network), now_us),
            Phase::Leaving {
                downed: false,
                then,
                ..
            } => {
                radio.ap_teardown(&self.cfg.paths);
                self.phase = Phase::Leaving {
                    until: now_us + self.cfg.timeouts.settle_us,
                    downed: true,
                    then,
                };
                Update::default()
            }
            Phase::Leaving { then, .. } => self.after_leaving(radio, then, now_us),
            Phase::Joining {
                until,
                ssid,
                next_check: _,
            } => self.check_joining(radio, until, ssid, now_us),
        }
    }

    fn check_starting(
        &mut self,
        radio: &mut Radio,
        until: u64,
        creds: Credentials,
        now_us: u64,
    ) -> Update {
        let view = self.log_view();
        if view.enabled || self.cfg.assume_enabled {
            let limit = self.cfg.timeouts.daemons_limit_s();
            if let Err(e) = radio.ap_serve(&self.cfg.paths, limit) {
                return self.fail_start(radio, e, now_us);
            }
            let portal = match Portal::bind(&self.cfg.binding, creds.token.clone(), creds.lang) {
                Ok(p) => PortalSlot(p),
                Err(e) => {
                    warn!("wifi-setup: cannot serve the page ({e})");
                    return self.fail_start(radio, ApError::Serve, now_us);
                }
            };
            info!("wifi-setup: serving");
            self.phase = Phase::Serving {
                until: now_us + self.cfg.timeouts.session_us,
                next_check: now_us + CHECK_US,
                creds,
                portal,
                clients: view.clients,
            };
            return self.publish_current(now_us);
        }
        if view.disabled || now_us >= until || !radio.ap_alive(&self.cfg.paths) {
            warn!("wifi-setup: the access point did not come up");
            return self.leave(radio, Then::restore(Some(ERR_AP)), now_us);
        }
        self.phase = Phase::Starting {
            until,
            next_check: now_us + CHECK_US,
            creds,
        };
        Update::default()
    }

    fn check_serving(
        &mut self,
        radio: &mut Radio,
        until: u64,
        creds: Credentials,
        mut portal: PortalSlot,
        clients: u32,
        now_us: u64,
    ) -> Update {
        if now_us >= until {
            info!("wifi-setup: timed out");
            return self.leave(radio, Then::restore(Some(ERR_TIMEOUT)), now_us);
        }
        let view = self.log_view();
        if view.disabled || !radio.ap_alive(&self.cfg.paths) {
            warn!("wifi-setup: the access point stopped");
            return self.leave(radio, Then::restore(Some(ERR_AP)), now_us);
        }
        portal.0.expire(now_us);
        self.phase = Phase::Serving {
            until,
            next_check: now_us + CHECK_US,
            creds,
            portal,
            clients: view.clients,
        };
        if view.clients == clients {
            Update::default()
        } else {
            self.publish_current(now_us)
        }
    }

    fn after_leaving(&mut self, radio: &mut Radio, then: Then, now_us: u64) -> Update {
        match then {
            Then::Restore { error, network } => {
                radio.release_wifi();
                let end = error.map_or(state(WifiSetupPhase::Off), failed);
                Update {
                    state: Some(WifiSetupState { network, ..end }),
                    finished: Some(Finished::Restore),
                }
            }
            Then::Join(network) => {
                radio.join_wifi(&network.conf, &self.tried_conf());
                self.phase = Phase::Joining {
                    until: now_us + self.cfg.timeouts.join_us,
                    next_check: now_us + CHECK_US,
                    ssid: network.ssid,
                };
                self.publish_current(now_us)
            }
        }
    }

    fn check_joining(
        &mut self,
        radio: &mut Radio,
        until: u64,
        ssid: String,
        now_us: u64,
    ) -> Update {
        // The access point's own address is no lease. The Glass was the DHCP server, never a client
        // of its own, so no other address can be from the session: what DHCP gives now is from
        // the network, even one that uses the same subnet. A DHCP client left from an earlier run,
        // which could renew an old lease, was stopped by its pid file when the session started.
        let leased = radio.wifi_ips().iter().any(|ip| {
            ip.parse::<std::net::Ipv4Addr>()
                .is_ok_and(|ip| ip != ap::AP_IP)
        });
        if leased {
            if let Err(e) = ap::install_conf(&self.tried_conf(), &self.cfg.wifi_conf) {
                // Saying "joined" would be saying it is kept. And the tried file has the password.
                warn!("wifi-setup: cannot store the network ({e})");
                let then = Then::Restore {
                    error: Some(ERR_FILES),
                    network: ssid,
                };
                return self.abandon_join(radio, then, now_us);
            }
            radio.finish_join(self.cfg.wifi_conf.clone());
            info!("wifi-setup: joined");
            return Update {
                state: Some(WifiSetupState {
                    network: ssid,
                    ..state(WifiSetupPhase::Joined)
                }),
                finished: Some(Finished::Joined),
            };
        }
        if now_us >= until {
            info!("wifi-setup: no lease from the network");
            let then = Then::Restore {
                error: Some(ERR_JOIN),
                network: ssid,
            };
            return self.abandon_join(radio, then, now_us);
        }
        self.phase = Phase::Joining {
            until,
            next_check: now_us + CHECK_US,
            ssid,
        };
        Update::default()
    }

    fn log_view(&mut self) -> ap::LogView {
        self.log.poll(&self.cfg.paths.hostapd_log())
    }
}

#[cfg(test)]
mod tests;
