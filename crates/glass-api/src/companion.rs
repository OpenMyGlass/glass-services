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
        if free && !self.phone_capabilities.iter().any(|c| c == CAP_DICTATION_FREE) {
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

    /// An app's `DictationConfirm`. `Err` when the request exists but is not a reply waiting
    /// for confirmation; unknown requests are ignored (they may have just expired).
    pub fn confirm(
        &mut self,
        client: ClientId,
        c: DictationConfirm,
        out: &mut Vec<Out>,
    ) -> Result<(), ()> {
        let Some(link) = self.find(client, c.request_id) else {
            return Ok(());
        };
        let l = &self.links[&link];
        if l.free || !l.final_seen {
            return Err(());
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
        Ok(())
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
