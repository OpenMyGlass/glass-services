//! The companion link provider and the dictation relay (docs/api.md, "Companion link
//! provider" and "Dictation"), as pure state used by [`Hub`](crate::Hub).
//!
//! One client may register as the provider: the process that holds the phone's bridge
//! session (the OpenMyGlass Glass app on Android, a daemon on plain Linux). Apps send
//! `DictationStart`; the relay gives each request a link id of its own, sends it to the
//! provider, and maps the provider's `DictationState` and `Transcript` back to the app's
//! request id. Audio never passes through here: the provider records it.

use crate::hub::ClientId;
use crate::proto::{
    CompanionState, DictationConfirm, DictationStart, DictationState, Transcript, dictation_state,
};
use std::collections::BTreeMap;

/// The phone forgets a request 60 s after its start, so the relay gives up after 55 s.
pub const DICTATION_EXPIRY_US: u64 = 55_000_000;

/// The phone capability that free dictation needs.
pub const CAP_DICTATION_FREE: &str = "dictation-free";

/// One relayed request.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub client: ClientId,
    pub request_id: u32,
    pub free: bool,
    pub started_us: u64,
    /// The final transcript was delivered (a reply then waits for `DictationConfirm`).
    pub final_seen: bool,
}

/// What the relay asks its owner to send.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// To an app: a state of its request (its own request id).
    State(ClientId, DictationState),
    /// To an app: a transcript of its request.
    Transcript(ClientId, Transcript),
    /// To the provider.
    Start(ClientId, DictationStart),
    Cancel(ClientId, u32),
    Confirm(ClientId, DictationConfirm),
}

/// Why a `DictationStart` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// No provider, or no phone connected.
    NoCompanion,
    /// Free dictation and the phone lacks `dictation-free`.
    Unsupported,
}

/// The provider and the relayed requests.
#[derive(Debug, Default)]
pub struct Relay {
    provider: Option<ClientId>,
    phone_connected: bool,
    phone_capabilities: Vec<String>,
    phone_name: String,
    changed_us: u64,
    links: BTreeMap<u32, Link>,
    next_link: u32,
}

fn state(request_id: u32, s: dictation_state::State, error: &str) -> DictationState {
    DictationState {
        request_id,
        state: s as i32,
        error: error.into(),
    }
}

impl Relay {
    pub fn provider(&self) -> Option<ClientId> {
        self.provider
    }

    pub fn links(&self) -> &BTreeMap<u32, Link> {
        &self.links
    }

    /// The `CompanionState` now (`snapshot` false).
    pub fn companion_state(&self) -> CompanionState {
        CompanionState {
            provider: self.provider.is_some(),
            phone_connected: self.provider.is_some() && self.phone_connected,
            phone_capabilities: if self.provider.is_some() {
                self.phone_capabilities.clone()
            } else {
                Vec::new()
            },
            phone_name: if self.provider.is_some() {
                self.phone_name.clone()
            } else {
                String::new()
            },
            snapshot: false,
            timestamp_us: self.changed_us,
        }
    }

    /// `client` registers as the provider; false if another one is registered.
    pub fn register(&mut self, client: ClientId, now_us: u64) -> bool {
        match self.provider {
            Some(p) if p != client => false,
            Some(_) => true,
            None => {
                self.provider = Some(client);
                self.phone_connected = false;
                self.phone_capabilities.clear();
                self.phone_name.clear();
                self.changed_us = now_us;
                true
            }
        }
    }

    /// The provider reports the phone link. Returns whether the state changed; requests
    /// fail with `link-lost` when the phone goes away.
    pub fn status(
        &mut self,
        connected: bool,
        capabilities: Vec<String>,
        name: String,
        now_us: u64,
        out: &mut Vec<Out>,
    ) -> bool {
        let changed = connected != self.phone_connected
            || capabilities != self.phone_capabilities
            || name != self.phone_name;
        if self.phone_connected && !connected {
            self.fail_all("link-lost", out);
        }
        self.phone_connected = connected;
        self.phone_capabilities = capabilities;
        self.phone_name = name;
        if changed {
            self.changed_us = now_us;
        }
        changed
    }

    /// A client went away. Returns whether it was the provider (the companion state then
    /// changed). Its own requests are cancelled at the provider.
    pub fn client_gone(&mut self, client: ClientId, now_us: u64, out: &mut Vec<Out>) -> bool {
        if self.provider == Some(client) {
            self.provider = None;
            self.phone_connected = false;
            self.phone_capabilities.clear();
            self.phone_name.clear();
            self.changed_us = now_us;
            self.fail_all("link-lost", out);
            return true;
        }
        let gone: Vec<u32> = self
            .links
            .iter()
            .filter(|(_, l)| l.client == client)
            .map(|(&id, _)| id)
            .collect();
        for id in gone {
            self.links.remove(&id);
            if let Some(p) = self.provider {
                out.push(Out::Cancel(p, id));
            }
        }
        false
    }

    fn fail_all(&mut self, why: &str, out: &mut Vec<Out>) {
        for (_, l) in std::mem::take(&mut self.links) {
            out.push(Out::State(
                l.client,
                state(l.request_id, dictation_state::State::Failed, why),
            ));
        }
    }

    fn find(&self, client: ClientId, request_id: u32) -> Option<u32> {
        self.links
            .iter()
            .find(|(_, l)| l.client == client && l.request_id == request_id)
            .map(|(&id, _)| id)
    }

    /// An app's valid `DictationStart` (the purpose was checked by the caller).
    pub fn start(
        &mut self,
        client: ClientId,
        d: DictationStart,
        now_us: u64,
        out: &mut Vec<Out>,
    ) -> Result<(), Refusal> {
        let Some(provider) = self.provider.filter(|_| self.phone_connected) else {
            return Err(Refusal::NoCompanion);
        };
        let free = d.purpose == "free";
        if free
            && !self
                .phone_capabilities
                .iter()
                .any(|c| c == CAP_DICTATION_FREE)
        {
            return Err(Refusal::Unsupported);
        }
        // A newer request of the same client replaces the older one.
        let older: Vec<(u32, u32)> = self
            .links
            .iter()
            .filter(|(_, l)| l.client == client)
            .map(|(&id, l)| (id, l.request_id))
            .collect();
        for (id, rid) in older {
            self.links.remove(&id);
            out.push(Out::Cancel(provider, id));
            if rid != d.request_id {
                out.push(Out::State(
                    client,
                    state(rid, dictation_state::State::Failed, "replaced"),
                ));
            }
        }
        self.next_link = self.next_link.wrapping_add(1).max(1);
        while self.links.contains_key(&self.next_link) {
            self.next_link = self.next_link.wrapping_add(1).max(1);
        }
        let link = self.next_link;
        self.links.insert(
            link,
            Link {
                client,
                request_id: d.request_id,
                free,
                started_us: now_us,
                final_seen: false,
            },
        );
        out.push(Out::Start(
            provider,
            DictationStart {
                request_id: link,
                ..d
            },
        ));
        Ok(())
    }

    /// An app's `DictationCancel`; unknown requests are ignored.
    pub fn cancel(&mut self, client: ClientId, request_id: u32, out: &mut Vec<Out>) {
        if let Some(link) = self.find(client, request_id) {
            self.links.remove(&link);
            if let Some(p) = self.provider {
                out.push(Out::Cancel(p, link));
            }
        }
    }

    /// An app's `DictationConfirm`. False when the request exists but is not a reply waiting
    /// for confirmation; unknown requests are ignored (they may have just expired).
    pub fn confirm(&mut self, client: ClientId, c: DictationConfirm, out: &mut Vec<Out>) -> bool {
        let Some(link) = self.find(client, c.request_id) else {
            return true;
        };
        let l = &self.links[&link];
        if l.free || !l.final_seen {
            return false;
        }
        let provider = self.provider.expect("links exist only with a provider");
        out.push(Out::Confirm(
            provider,
            DictationConfirm {
                request_id: link,
                send: c.send,
            },
        ));
        if !c.send {
            self.links.remove(&link);
        }
        true
    }

    /// The provider's `DictationState` for a link. `STATE_DONE` is ignored: the relay sends
    /// it itself after the final transcript.
    pub fn provider_state(&mut self, s: DictationState, out: &mut Vec<Out>) {
        let Some(l) = self.links.get(&s.request_id) else {
            return;
        };
        use dictation_state::State as St;
        let st = St::try_from(s.state).unwrap_or(St::Unspecified);
        let (client, rid) = (l.client, l.request_id);
        match st {
            St::Unspecified | St::Done => return,
            St::Listening | St::Processing => {}
            St::Failed | St::Sent => {
                self.links.remove(&s.request_id);
            }
        }
        out.push(Out::State(
            client,
            DictationState {
                request_id: rid,
                ..s
            },
        ));
    }

    /// The provider's `Transcript` for a link. After the final one the app gets
    /// `STATE_DONE`; a free request then ends, a reply waits for `DictationConfirm`.
    pub fn provider_transcript(&mut self, t: Transcript, out: &mut Vec<Out>) {
        let Some(l) = self.links.get_mut(&t.request_id) else {
            return;
        };
        if l.final_seen {
            return;
        }
        let (client, rid, free) = (l.client, l.request_id, l.free);
        let fin = t.r#final;
        if fin {
            l.final_seen = true;
        }
        out.push(Out::Transcript(
            client,
            Transcript {
                request_id: rid,
                ..t
            },
        ));
        if fin {
            out.push(Out::State(
                client,
                state(rid, dictation_state::State::Done, ""),
            ));
            if free {
                self.links.remove(&t.request_id);
            }
        }
    }

    /// The earliest expiry of a relayed request.
    pub fn next_deadline(&self) -> Option<u64> {
        self.links
            .values()
            .map(|l| l.started_us + DICTATION_EXPIRY_US)
            .min()
    }

    /// Ends the requests older than [`DICTATION_EXPIRY_US`] with `expired`.
    pub fn on_timeout(&mut self, now_us: u64, out: &mut Vec<Out>) {
        let old: Vec<u32> = self
            .links
            .iter()
            .filter(|(_, l)| now_us >= l.started_us + DICTATION_EXPIRY_US)
            .map(|(&id, _)| id)
            .collect();
        for id in old {
            let l = self.links.remove(&id).expect("listed");
            if let Some(p) = self.provider {
                out.push(Out::Cancel(p, id));
            }
            out.push(Out::State(
                l.client,
                state(l.request_id, dictation_state::State::Failed, "expired"),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::hub::{ClientId, Effect, Hub};
    use crate::proto::{
        ClientFrame, CompanionRegister, CompanionStatus, DictationCancel, DictationConfirm,
        DictationStart, DictationState, ErrorCode, Hello, Subscribe, Topic, Transcript,
        client_frame::Body as C, dictation_state::State as St, service_frame::Body as S,
    };

    fn cf(body: C) -> ClientFrame {
        ClientFrame {
            seq: 7,
            body: Some(body),
        }
    }

    fn client(hub: &mut Hub, topics: &[Topic]) -> ClientId {
        let id = hub.add_client();
        hub.on_frame(
            id,
            cf(C::Hello(Hello {
                api_version: 1,
                client_name: "t".into(),
            })),
        );
        hub.on_frame(
            id,
            cf(C::Subscribe(Subscribe {
                topics: topics.iter().map(|&t| t as i32).collect(),
            })),
        );
        id
    }

    fn sends(fx: &[Effect]) -> Vec<(ClientId, S)> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Send(id, f) => Some((*id, f.body.clone().unwrap())),
                _ => None,
            })
            .collect()
    }

    fn err(fx: &[Effect]) -> Option<ErrorCode> {
        sends(fx).into_iter().find_map(|(_, b)| match b {
            S::Error(e) => Some(ErrorCode::try_from(e.code).unwrap()),
            _ => None,
        })
    }

    fn status(connected: bool, caps: &[&str]) -> C {
        C::CompanionStatus(CompanionStatus {
            phone_connected: connected,
            phone_capabilities: caps.iter().map(|s| s.to_string()).collect(),
            phone_name: "Pixel".into(),
        })
    }

    fn start(rid: u32, purpose: &str, key: &str) -> C {
        C::DictationStart(DictationStart {
            request_id: rid,
            purpose: purpose.into(),
            notification_key: key.into(),
            action_index: 1,
        })
    }

    /// A hub with a provider (phone connected, free dictation) and an app.
    fn setup() -> (Hub, ClientId, ClientId) {
        let mut hub = Hub::new("t", &["dictation", "companion"]);
        hub.set_time(1_000_000);
        let provider = client(&mut hub, &[]);
        let app = client(&mut hub, &[Topic::Companion]);
        assert!(
            err(&hub.on_frame(
                provider,
                cf(C::CompanionRegister(CompanionRegister { name: "p".into() }))
            ))
            .is_none()
        );
        hub.on_frame(provider, cf(status(true, &["dictation-free"])));
        (hub, provider, app)
    }

    /// Starts `rid` for `app` and returns the link id the provider got.
    fn started(hub: &mut Hub, provider: ClientId, app: ClientId, rid: u32, purpose: &str) -> u32 {
        let fx = hub.on_frame(app, cf(start(rid, purpose, "k")));
        let s = sends(&fx);
        assert_eq!(s.len(), 1, "{s:?}");
        match &s[0] {
            (p, S::ProviderDictationStart(d)) if *p == provider => {
                assert_eq!(d.purpose, purpose);
                assert_ne!(d.request_id, 0);
                d.request_id
            }
            other => panic!("{other:?}"),
        }
    }

    fn pstate(link: u32, st: St, error: &str) -> C {
        C::ProviderDictationState(DictationState {
            request_id: link,
            state: st as i32,
            error: error.into(),
        })
    }

    fn ptext(link: u32, text: &str, fin: bool) -> C {
        C::ProviderTranscript(Transcript {
            request_id: link,
            text: text.into(),
            r#final: fin,
        })
    }

    fn states(fx: &[Effect], to: ClientId) -> Vec<(u32, St, String)> {
        sends(fx)
            .into_iter()
            .filter_map(|(id, b)| match b {
                S::DictationState(d) if id == to => {
                    Some((d.request_id, St::try_from(d.state).unwrap(), d.error))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn companion_snapshot_and_broadcasts() {
        let mut hub = Hub::new("t", &[]);
        let app = client(&mut hub, &[]);
        let fx = hub.on_frame(
            app,
            cf(C::Subscribe(Subscribe {
                topics: vec![Topic::Companion as i32],
            })),
        );
        match &sends(&fx)[..] {
            [(_, S::CompanionState(c))] => {
                assert!(c.snapshot && !c.provider && !c.phone_connected)
            }
            other => panic!("{other:?}"),
        }
        let p = client(&mut hub, &[]);
        let fx = hub.on_frame(
            p,
            cf(C::CompanionRegister(CompanionRegister { name: "p".into() })),
        );
        match &sends(&fx)[..] {
            [(id, S::CompanionState(c))] if *id == app => {
                assert!(!c.snapshot && c.provider && !c.phone_connected)
            }
            other => panic!("{other:?}"),
        }
        let fx = hub.on_frame(p, cf(status(true, &["dictation-free"])));
        match &sends(&fx)[..] {
            [(_, S::CompanionState(c))] => {
                assert!(c.phone_connected);
                assert_eq!(c.phone_name, "Pixel");
                assert_eq!(c.phone_capabilities, vec!["dictation-free".to_string()]);
            }
            other => panic!("{other:?}"),
        }
        // The same status again changes nothing.
        assert!(sends(&hub.on_frame(p, cf(status(true, &["dictation-free"])))).is_empty());
        // The provider goes away.
        let fx = hub.remove_client(p);
        match &sends(&fx)[..] {
            [(_, S::CompanionState(c))] => assert!(!c.provider && !c.phone_connected),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn one_provider_at_a_time_and_only_it_reports() {
        let (mut hub, provider, app) = setup();
        let other = client(&mut hub, &[]);
        let reg = cf(C::CompanionRegister(CompanionRegister { name: "q".into() }));
        assert_eq!(
            err(&hub.on_frame(other, reg.clone())),
            Some(ErrorCode::Unavailable)
        );
        // The same provider again is fine and silent.
        assert!(sends(&hub.on_frame(provider, reg.clone())).is_empty());
        assert_eq!(
            err(&hub.on_frame(app, cf(status(true, &[])))),
            Some(ErrorCode::InvalidArgument)
        );
        assert_eq!(
            err(&hub.on_frame(app, cf(ptext(1, "x", true)))),
            Some(ErrorCode::InvalidArgument)
        );
        hub.remove_client(provider);
        assert!(err(&hub.on_frame(other, reg)).is_none());
    }

    #[test]
    fn refusals() {
        let mut hub = Hub::new("t", &[]);
        let app = client(&mut hub, &[]);
        assert_eq!(
            err(&hub.on_frame(app, cf(start(1, "free", "")))),
            Some(ErrorCode::NoCompanion)
        );
        assert_eq!(
            err(&hub.on_frame(app, cf(start(1, "song", "")))),
            Some(ErrorCode::InvalidArgument)
        );
        assert_eq!(
            err(&hub.on_frame(app, cf(start(1, "notification-reply", "")))),
            Some(ErrorCode::InvalidArgument)
        );
        // A provider without a phone.
        let p = client(&mut hub, &[]);
        hub.on_frame(
            p,
            cf(C::CompanionRegister(CompanionRegister { name: "p".into() })),
        );
        assert_eq!(
            err(&hub.on_frame(app, cf(start(1, "free", "")))),
            Some(ErrorCode::NoCompanion)
        );
        // A phone without free dictation: replies work, free does not.
        hub.on_frame(p, cf(status(true, &[])));
        assert_eq!(
            err(&hub.on_frame(app, cf(start(1, "free", "")))),
            Some(ErrorCode::Unsupported)
        );
        assert!(err(&hub.on_frame(app, cf(start(2, "notification-reply", "k")))).is_none());
    }

    #[test]
    fn free_dictation_round_trip() {
        let (mut hub, provider, app) = setup();
        let link = started(&mut hub, provider, app, 42, "free");
        let fx = hub.on_frame(provider, cf(pstate(link, St::Listening, "")));
        assert_eq!(states(&fx, app), vec![(42, St::Listening, String::new())]);
        let fx = hub.on_frame(provider, cf(ptext(link, "hello", false)));
        match &sends(&fx)[..] {
            [(id, S::Transcript(t))] if *id == app => {
                assert_eq!(
                    (t.request_id, t.text.as_str(), t.r#final),
                    (42, "hello", false)
                )
            }
            other => panic!("{other:?}"),
        }
        // A provider DONE is ignored; glassd sends DONE after the final transcript.
        assert!(sends(&hub.on_frame(provider, cf(pstate(link, St::Done, "")))).is_empty());
        let fx = hub.on_frame(provider, cf(ptext(link, "hello world", true)));
        let s = sends(&fx);
        assert!(matches!(&s[0], (_, S::Transcript(t)) if t.r#final && t.request_id == 42));
        assert_eq!(states(&fx, app), vec![(42, St::Done, String::new())]);
        // The request is over: later frames for the link are ignored.
        assert!(hub.relay().links().is_empty());
        assert!(sends(&hub.on_frame(provider, cf(ptext(link, "late", true)))).is_empty());
        assert!(hub.next_deadline().is_none());
    }

    #[test]
    fn reply_waits_for_confirm_and_sent() {
        let (mut hub, provider, app) = setup();
        let link = started(&mut hub, provider, app, 5, "notification-reply");
        // Confirm before the final transcript is an error.
        let confirm = |send| {
            cf(C::DictationConfirm(DictationConfirm {
                request_id: 5,
                send,
            }))
        };
        assert_eq!(
            err(&hub.on_frame(app, confirm(true))),
            Some(ErrorCode::InvalidArgument)
        );
        let fx = hub.on_frame(provider, cf(ptext(link, "on my way", true)));
        assert_eq!(states(&fx, app), vec![(5, St::Done, String::new())]);
        let fx = hub.on_frame(app, confirm(true));
        match &sends(&fx)[..] {
            [(id, S::ProviderDictationConfirm(c))] if *id == provider => {
                assert_eq!((c.request_id, c.send), (link, true))
            }
            other => panic!("{other:?}"),
        }
        let fx = hub.on_frame(provider, cf(pstate(link, St::Sent, "")));
        assert_eq!(states(&fx, app), vec![(5, St::Sent, String::new())]);
        assert!(hub.relay().links().is_empty());
    }

    #[test]
    fn reply_discarded_ends_the_request() {
        let (mut hub, provider, app) = setup();
        let link = started(&mut hub, provider, app, 5, "notification-reply");
        hub.on_frame(provider, cf(ptext(link, "x", true)));
        hub.on_frame(
            app,
            cf(C::DictationConfirm(DictationConfirm {
                request_id: 5,
                send: false,
            })),
        );
        assert!(hub.relay().links().is_empty());
    }

    #[test]
    fn two_apps_same_request_id_do_not_collide() {
        let (mut hub, provider, app) = setup();
        let app2 = client(&mut hub, &[]);
        let l1 = started(&mut hub, provider, app, 1, "free");
        let l2 = started(&mut hub, provider, app2, 1, "free");
        assert_ne!(l1, l2);
        let fx = hub.on_frame(provider, cf(pstate(l2, St::Failed, "no-speech")));
        assert!(states(&fx, app).is_empty());
        assert_eq!(states(&fx, app2), vec![(1, St::Failed, "no-speech".into())]);
        assert_eq!(hub.relay().links().len(), 1);
    }

    #[test]
    fn newer_request_replaces_older() {
        let (mut hub, provider, app) = setup();
        let l1 = started(&mut hub, provider, app, 1, "free");
        let fx = hub.on_frame(app, cf(start(2, "free", "")));
        let s = sends(&fx);
        assert!(s.contains(&(
            provider,
            S::ProviderDictationCancel(DictationCancel { request_id: l1 })
        )));
        assert_eq!(states(&fx, app), vec![(1, St::Failed, "replaced".into())]);
        assert!(
            s.iter()
                .any(|(id, b)| *id == provider && matches!(b, S::ProviderDictationStart(_)))
        );
        assert_eq!(hub.relay().links().len(), 1);
        // The same request id again: cancelled at the provider, no "replaced".
        let fx = hub.on_frame(app, cf(start(2, "free", "")));
        assert!(states(&fx, app).is_empty());
        assert_eq!(hub.relay().links().len(), 1);
    }

    #[test]
    fn cancel_and_app_disconnect_cancel_at_the_provider() {
        let (mut hub, provider, app) = setup();
        let l1 = started(&mut hub, provider, app, 1, "free");
        let fx = hub.on_frame(
            app,
            cf(C::DictationCancel(DictationCancel { request_id: 1 })),
        );
        assert_eq!(
            sends(&fx),
            vec![(
                provider,
                S::ProviderDictationCancel(DictationCancel { request_id: l1 })
            )]
        );
        // Unknown: ignored.
        assert!(
            sends(&hub.on_frame(
                app,
                cf(C::DictationCancel(DictationCancel { request_id: 9 }))
            ))
            .is_empty()
        );
        let l2 = started(&mut hub, provider, app, 3, "free");
        let fx = hub.remove_client(app);
        assert_eq!(
            sends(&fx),
            vec![(
                provider,
                S::ProviderDictationCancel(DictationCancel { request_id: l2 })
            )]
        );
    }

    #[test]
    fn link_lost_when_the_phone_or_the_provider_goes() {
        let (mut hub, provider, app) = setup();
        started(&mut hub, provider, app, 1, "free");
        let fx = hub.on_frame(provider, cf(status(false, &[])));
        assert_eq!(states(&fx, app), vec![(1, St::Failed, "link-lost".into())]);
        hub.on_frame(provider, cf(status(true, &["dictation-free"])));
        started(&mut hub, provider, app, 2, "free");
        let fx = hub.remove_client(provider);
        assert_eq!(states(&fx, app), vec![(2, St::Failed, "link-lost".into())]);
        assert_eq!(
            err(&hub.on_frame(app, cf(start(3, "free", "")))),
            Some(ErrorCode::NoCompanion)
        );
    }

    #[test]
    fn requests_expire_after_55_s() {
        let (mut hub, provider, app) = setup();
        let link = started(&mut hub, provider, app, 1, "free");
        assert_eq!(hub.next_deadline(), Some(1_000_000 + 55_000_000));
        assert!(hub.on_timeout(55_999_999).is_empty());
        let fx = hub.on_timeout(56_000_000);
        assert_eq!(states(&fx, app), vec![(1, St::Failed, "expired".into())]);
        assert!(sends(&fx).contains(&(
            provider,
            S::ProviderDictationCancel(DictationCancel { request_id: link })
        )));
        assert!(hub.next_deadline().is_none());
    }
}
