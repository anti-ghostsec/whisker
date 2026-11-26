//! Voice calls: one at a time, between two people, in the chat they share.
//!
//! Off until turned on in Settings. Private by design (see
//! `whisker_core::calls`): the sound only goes through the account's
//! server's relay, so neither side learns the other's address, and an
//! account that connects through a proxy doesn't call at all (a call's
//! sound can't go through the proxy, and would go around it). The voice is
//! changed before it leaves, unless that's turned off.
//!
//! How a call goes: the caller sends an invite (its side of the
//! connection), the other side's devices ring; the one that picks up sends
//! an answer, the caller tells the others which device has it, and the two
//! connect. Either side hangs up.

use crate::{Ctl, RoomKey, call_audio};
use matrix_sdk::ruma::{MilliSecondsSinceUnixEpoch, OwnedRoomId, OwnedUserId};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use whisker_core::AccountId;
use whisker_core::call_media::{Link, LinkEvent, Route};
use whisker_core::calls::{self, Candidate, RING_MS, Said, Signal};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// We called; it's ringing there.
    Calling,
    /// They called; it's ringing here.
    Ringing,
    /// Picked up; the two sides are finding each other.
    Connecting,
    Talking,
}

struct Call {
    key: RoomKey,
    id: String,
    /// This device's side of the call, and the other side's device (once
    /// one of theirs is known: the one calling, or the one that picked up).
    party: String,
    their_party: Option<String>,
    /// The other person, as shown.
    who: String,
    stage: Stage,
    link: Option<Arc<Link>>,
    /// Their side of the connection, from the invite, until picked up.
    offer: Option<String>,
    /// Ways to reach them that came before the connection could take them.
    waiting: Vec<Candidate>,
    /// Their side of the connection is in: more ways to reach them can go straight in.
    described: bool,
    muted: Arc<AtomicBool>,
    /// Which call this is, of all there have been: what's started for one
    /// call (timers, the connection's reports) leaves the next alone.
    serial: u64,
}

static NOW: Mutex<Option<Call>> = Mutex::new(None);
static SERIAL: AtomicU64 = AtomicU64::new(0);

/// Whether call `serial` is still the one going on.
fn current(serial: u64) -> bool {
    NOW.lock().unwrap().as_ref().is_some_and(|c| c.serial == serial)
}

impl Ctl {
    fn calls_on(&self) -> bool {
        self.settings.lock().unwrap().voice_calls
    }

    /// Shows the call (or that there's none) in the window.
    fn call_show(&self) {
        let shown = NOW.lock().unwrap().as_ref().map(|c| (c.stage, c.who.clone(), c.muted.load(Ordering::Relaxed)));
        self.ui(move |u| match shown {
            Some((stage, who, muted)) => {
                let n = match stage {
                    Stage::Calling => 1,
                    Stage::Ringing => 2,
                    Stage::Connecting => 3,
                    Stage::Talking => 4,
                };
                if u.get_call_stage() != n {
                    u.set_call_seconds(0);
                }
                u.set_call_name(crate::s(who));
                u.set_call_muted(muted);
                u.set_call_stage(n);
            }
            None => u.set_call_stage(0),
        });
    }

    /// The room a call is in, to say things in.
    async fn call_room(&self, key: &RoomKey) -> Option<matrix_sdk::Room> {
        self.mgr.client(key.0).await?.get_room(&key.1)
    }

    async fn call_say(&self, key: &RoomKey, call_id: &str, party: &str, signal: Signal) {
        let Some(room) = self.call_room(key).await else { return };
        let said = Said { call_id: call_id.to_owned(), party_id: party.to_owned(), signal };
        if let Err(e) = calls::send(&room, &said).await {
            tracing::warn!("call: couldn't send {:?}: {e:#}", said.signal);
        }
    }

    /// Why a call can't be made or taken in `room` on account `account`, if it can't.
    async fn call_barred(&self, account: AccountId, room: &matrix_sdk::Room) -> Option<&'static str> {
        if !crate::voice::available() {
            return Some("Calls need a microphone Whisker can use (on Linux, ffmpeg).");
        }
        if self.mgr.proxy_of(account).await.is_some() {
            return Some("This account connects through a proxy. A call's sound can't go through it, so this account doesn't call.");
        }
        if room.joined_members_count() != 2 {
            return Some("Calls are for chats between two people.");
        }
        None
    }

    /// Ends the call that's going on (`serial`: only if it's that one),
    /// here: the sound stops and the connection closes. `tell`: what to
    /// say to the other side, if anything. `note`: what to say here.
    async fn call_end(&self, serial: Option<u64>, tell: Option<Signal>, note: Option<String>) {
        let call = {
            let mut now = NOW.lock().unwrap();
            match (now.as_ref(), serial) {
                (Some(c), Some(s)) if c.serial != s => return,
                (None, _) => return,
                _ => now.take(),
            }
        };
        let Some(call) = call else { return };
        call_audio::stop();
        self.call_show();
        // With nothing to say, "… is calling." doesn't stay behind.
        self.status(note.unwrap_or_default());
        if let Some(signal) = tell {
            self.call_say(&call.key, &call.id, &call.party, signal).await;
        }
        if let Some(link) = call.link {
            link.close().await;
        }
    }

    /// The phone button: calls the other person in the open chat.
    pub(crate) async fn call_start(&self) {
        if !self.calls_on() {
            return;
        }
        if NOW.lock().unwrap().is_some() {
            return self.status("There's a call going on already.");
        }
        let Some((key, room)) = self.current_room().await else { return };
        if let Some(why) = self.call_barred(key.0, &room).await {
            return self.status(why);
        }
        #[cfg(target_os = "android")]
        if !crate::droid::has_mic() {
            crate::droid::request_mic();
            return self.status("Allow Whisker to use the microphone, then call again.");
        }
        let who = room.cached_display_name().map(|n| n.to_string()).unwrap_or_else(|| "them".to_owned());
        let serial = SERIAL.fetch_add(1, Ordering::Relaxed) + 1;
        let (id, party) = (calls::new_id(), calls::new_id());
        *NOW.lock().unwrap() = Some(Call {
            key: key.clone(),
            id: id.clone(),
            party: party.clone(),
            their_party: None,
            who: who.clone(),
            stage: Stage::Calling,
            link: None,
            offer: None,
            waiting: Vec::new(),
            described: false,
            muted: Arc::new(AtomicBool::new(false)),
            serial,
        });
        self.call_show();
        let offer = match self.call_link(key.0, serial).await {
            Ok(link) => link.offer().await,
            Err(e) => Err(e),
        };
        let offer = match offer {
            Ok(o) => o,
            Err(e) => return self.call_end(Some(serial), None, Some(format!("Couldn't call: {e:#}"))).await,
        };
        // Cancelled while it was being set up.
        if !current(serial) {
            return;
        }
        self.call_say(&key, &id, &party, Signal::Invite { sdp: offer, lifetime: RING_MS }).await;
        // Nobody picks up: it stops ringing.
        let c = self.clone();
        self.spawn(async move {
            tokio::time::sleep(Duration::from_millis(RING_MS)).await;
            let ringing = NOW.lock().unwrap().as_ref().is_some_and(|x| x.serial == serial && x.stage == Stage::Calling);
            if ringing {
                c.call_end(Some(serial), Some(Signal::Hangup { reason: Some("invite_timeout".into()) }), Some(format!("{who} didn't pick up."))).await;
            }
        });
    }

    /// Sets up call `serial`'s connection through the account's server's
    /// relay, and follows what it reports.
    async fn call_link(&self, account: AccountId, serial: u64) -> anyhow::Result<Arc<Link>> {
        let client = self.mgr.client(account).await.ok_or_else(|| anyhow::anyhow!("the account isn't connected"))?;
        let relay = calls::relay(&client).await.map_err(|e| anyhow::anyhow!("no relay for calls from this account's server, so a call can't be made privately ({e:#})"))?;
        let (link, mut reports) = Link::new(Route::Relay(relay)).await?;
        let link = Arc::new(link);
        match NOW.lock().unwrap().as_mut() {
            Some(c) if c.serial == serial => c.link = Some(link.clone()),
            _ => anyhow::bail!("the call was ended"),
        }
        let c = self.clone();
        self.spawn(async move {
            while let Some(report) = reports.recv().await {
                if !current(serial) {
                    break;
                }
                match report {
                    LinkEvent::Candidate(candidate) => {
                        let to = NOW.lock().unwrap().as_ref().filter(|x| x.serial == serial).map(|x| (x.key.clone(), x.id.clone(), x.party.clone()));
                        if let Some((key, id, party)) = to {
                            c.call_say(&key, &id, &party, Signal::Candidates(vec![candidate])).await;
                        }
                    }
                    LinkEvent::Connected => c.call_connected(serial),
                    LinkEvent::Interrupted => c.status("The call's connection was interrupted. Waiting for it to come back…"),
                    LinkEvent::Ended => {
                        c.call_end(Some(serial), Some(Signal::Hangup { reason: Some("ice_failed".into()) }), Some("The call's connection was lost.".into())).await;
                        break;
                    }
                }
            }
        });
        Ok(link)
    }

    /// The two sides found each other: the microphone and speakers start.
    fn call_connected(&self, serial: u64) {
        let started = {
            let mut now = NOW.lock().unwrap();
            match now.as_mut().filter(|x| x.serial == serial && x.stage != Stage::Talking) {
                Some(call) => {
                    call.stage = Stage::Talking;
                    call.link.clone().map(|l| (l, call.muted.clone()))
                }
                None => None,
            }
        };
        let Some((link, muted)) = started else { return };
        self.call_show();
        let change = self.settings.lock().unwrap().change_voice;
        if let Err(e) = call_audio::start(&self.rt, link, change, muted) {
            let c = self.clone();
            self.spawn(async move { c.call_end(Some(serial), Some(Signal::Hangup { reason: Some("user_media_failed".into()) }), Some(format!("The call ended: {e}"))).await });
        }
    }

    /// Answer: picks up the call that's ringing here.
    pub(crate) async fn call_answer(&self) {
        #[cfg(target_os = "android")]
        if !crate::droid::has_mic() {
            crate::droid::request_mic();
            return self.status("Allow Whisker to use the microphone, then answer again.");
        }
        let picked = {
            let mut now = NOW.lock().unwrap();
            match now.as_mut().filter(|c| c.stage == Stage::Ringing) {
                Some(c) => {
                    c.stage = Stage::Connecting;
                    c.offer.take().map(|offer| (c.key.clone(), c.id.clone(), c.party.clone(), c.serial, offer))
                }
                None => None,
            }
        };
        let Some((key, id, party, serial, offer)) = picked else { return };
        self.call_show();
        let answer = match self.call_link(key.0, serial).await {
            Ok(link) => link.accept(&offer).await.map(|a| (link, a)),
            Err(e) => Err(e),
        };
        let (link, answer) = match answer {
            Ok(x) => x,
            Err(e) => return self.call_end(Some(serial), Some(Signal::Hangup { reason: Some("ice_failed".into()) }), Some(format!("Couldn't answer: {e:#}"))).await,
        };
        if !current(serial) {
            return;
        }
        self.call_say(&key, &id, &party, Signal::Answer { sdp: answer }).await;
        self.call_described(serial, &link).await;
    }

    /// Their side of the connection is in: the ways to reach them that
    /// were kept go in too.
    async fn call_described(&self, serial: u64, link: &Link) {
        let kept = match NOW.lock().unwrap().as_mut().filter(|c| c.serial == serial) {
            Some(c) => {
                c.described = true;
                std::mem::take(&mut c.waiting)
            }
            None => return,
        };
        for candidate in kept {
            let _ = link.reach(&candidate).await;
        }
    }

    /// Hang up, Decline or Cancel.
    pub(crate) async fn call_hangup(&self) {
        let ringing = NOW.lock().unwrap().as_ref().map(|c| c.stage == Stage::Ringing);
        let tell = match ringing {
            Some(true) => Signal::Reject,
            Some(false) => Signal::Hangup { reason: Some("user_hangup".into()) },
            None => return,
        };
        self.call_end(None, Some(tell), None).await;
    }

    /// Turns the microphone off or on for the other side.
    pub(crate) fn call_mute(&self) {
        if let Some(c) = NOW.lock().unwrap().as_ref() {
            c.muted.fetch_xor(true, Ordering::Relaxed);
        }
        self.call_show();
    }

    /// Something was said about a call in a room (see `ClientEvent::Call`).
    pub(crate) async fn call_said(&self, account: AccountId, room_id: OwnedRoomId, sender: OwnedUserId, said: Said, ts: MilliSecondsSinceUnixEpoch) {
        let key: RoomKey = (account, room_id);
        let Some(client) = self.mgr.client(account).await else { return };
        // From this account: another of its devices.
        let own = client.user_id().is_some_and(|me| me == sender);
        // The call going on, if this is about it.
        let about = NOW.lock().unwrap().as_ref().filter(|c| c.key == key && c.id == said.call_id).map(|c| (c.serial, c.stage, c.their_party.clone(), c.party.clone(), c.who.clone()));
        match said.signal {
            Signal::Invite { sdp, lifetime } => {
                if own || !self.calls_on() || about.is_some() {
                    return;
                }
                // Catching up on what was said while away: it rang long ago.
                let age = u64::from(MilliSecondsSinceUnixEpoch::now().0).saturating_sub(u64::from(ts.0));
                if age >= lifetime {
                    return;
                }
                let Some(room) = client.get_room(&key.1) else { return };
                if self.call_barred(account, &room).await.is_some() {
                    return;
                }
                // Already on a call here. Nothing is said back: a decline
                // would stop this account's other devices ringing too, and
                // the call going on may be this very one, placed from
                // another account signed in on this device.
                let busy = NOW.lock().unwrap().as_ref().map(|c| c.id == said.call_id);
                match busy {
                    Some(true) => return,
                    Some(false) => {
                        let who = room.cached_display_name().map(|n| n.to_string()).unwrap_or_else(|| sender.to_string());
                        return self.status(format!("{who} tried to call during this call."));
                    }
                    None => {}
                }
                let party = calls::new_id();
                let who = room.cached_display_name().map(|n| n.to_string()).unwrap_or_else(|| sender.to_string());
                let serial = SERIAL.fetch_add(1, Ordering::Relaxed) + 1;
                *NOW.lock().unwrap() = Some(Call {
                    key,
                    id: said.call_id,
                    party,
                    their_party: Some(said.party_id),
                    who: who.clone(),
                    stage: Stage::Ringing,
                    link: None,
                    offer: Some(sdp),
                    waiting: Vec::new(),
                    described: false,
                    muted: Arc::new(AtomicBool::new(false)),
                    serial,
                });
                self.call_show();
                self.status(format!("{who} is calling."));
                // A notification too (no name in it), unless they're off.
                if crate::notify::Detail::from_index(self.settings.lock().unwrap().notify_level) != crate::notify::Detail::Off {
                    let _ = tokio::task::spawn_blocking(|| crate::notify::show("Whisker", "Incoming call", 0, false)).await;
                }
                // It stops ringing when the caller would have given up.
                let c = self.clone();
                self.spawn(async move {
                    tokio::time::sleep(Duration::from_millis(lifetime - age)).await;
                    let ringing = NOW.lock().unwrap().as_ref().is_some_and(|x| x.serial == serial && x.stage == Stage::Ringing);
                    if ringing {
                        c.call_end(Some(serial), None, Some(format!("Missed a call from {who}."))).await;
                    }
                });
            }
            Signal::Candidates(list) => {
                let Some((serial, _, theirs, ..)) = about else { return };
                // Only from the device this call is with.
                if own || theirs.is_some_and(|t| t != said.party_id) {
                    return;
                }
                let link = match NOW.lock().unwrap().as_mut().filter(|c| c.serial == serial) {
                    Some(c) if c.described => c.link.clone(),
                    Some(c) => {
                        c.waiting.extend(list.iter().cloned());
                        c.waiting.truncate(100);
                        None
                    }
                    None => None,
                };
                if let Some(link) = link {
                    for candidate in &list {
                        let _ = link.reach(candidate).await;
                    }
                }
            }
            Signal::Answer { sdp } => {
                let Some((serial, stage, ..)) = about else { return };
                if own {
                    // Picked up on another of this account's devices.
                    if stage == Stage::Ringing {
                        self.call_end(Some(serial), None, Some("The call was answered on another device.".into())).await;
                    }
                    return;
                }
                // The first device of theirs to pick up has the call.
                let link = match NOW.lock().unwrap().as_mut().filter(|c| c.serial == serial && c.stage == Stage::Calling) {
                    Some(c) => {
                        c.stage = Stage::Connecting;
                        c.their_party = Some(said.party_id.clone());
                        // What came from their other devices is no use now.
                        c.waiting.clear();
                        c.link.clone()
                    }
                    None => return,
                };
                let Some(link) = link else { return };
                self.call_show();
                if let Err(e) = link.answered(&sdp).await {
                    return self.call_end(Some(serial), Some(Signal::Hangup { reason: Some("ice_failed".into()) }), Some(format!("The call couldn't connect: {e:#}"))).await;
                }
                let Some((_, _, _, party, _)) = about else { return };
                self.call_say(&key, &said.call_id, &party, Signal::SelectAnswer { party: said.party_id }).await;
                self.call_described(serial, &link).await;
            }
            Signal::SelectAnswer { party } => {
                // The caller picked another of this account's devices.
                if let Some((serial, stage, _, mine, _)) = about
                    && !own
                    && stage != Stage::Calling
                    && party != mine
                {
                    self.call_end(Some(serial), None, Some("The call was answered on another device.".into())).await;
                }
            }
            Signal::Reject => {
                let Some((serial, stage, _, _, who)) = about else { return };
                match (own, stage) {
                    (true, Stage::Ringing) => self.call_end(Some(serial), None, Some("The call was declined on another device.".into())).await,
                    (false, Stage::Calling) => self.call_end(Some(serial), None, Some(format!("{who} declined the call."))).await,
                    _ => {}
                }
            }
            Signal::Hangup { .. } => {
                let Some((serial, stage, theirs, _, who)) = about else { return };
                // The other side, from the device this call is with (any of
                // theirs while it only rings).
                if own || (stage != Stage::Ringing && stage != Stage::Calling && theirs.is_some_and(|t| t != said.party_id)) {
                    return;
                }
                let note = match stage {
                    Stage::Ringing => format!("Missed a call from {who}."),
                    Stage::Calling => format!("{who} isn't taking the call."),
                    _ => format!("{who} ended the call."),
                };
                self.call_end(Some(serial), None, Some(note)).await;
            }
        }
    }
}

/// Ends the call at once, without a word to the other side: for when
/// Whisker locks (the sound stops with everything else; the other side
/// sees the connection go).
pub(crate) fn drop_now() {
    call_audio::stop();
    if let Some(call) = NOW.lock().unwrap().take()
        && let Some(link) = call.link
    {
        // Closing gives the relay's address back; it needs the runtime.
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { link.close().await });
        }
    }
}
