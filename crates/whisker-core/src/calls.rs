//! Voice calls between two people, the way Matrix has long done them
//! (`m.call.invite` and the events that follow, version 1): what the two
//! sides say to each other to set a call up, as events in their chat (so
//! encrypted like its messages, in an encrypted chat).
//!
//! Private by design: the sound only ever travels through the relay (TURN
//! server) the account's own server names, so neither side learns the
//! other's address on the internet. Nothing here offers an address of this
//! device's own ([`relay_only`] takes any out of what's sent, whatever
//! made it), and with no relay there's no call.

use anyhow::{Result, bail};
use matrix_sdk::Client;
use matrix_sdk::room::Room;
use serde_json::{Value, json};

const INVITE: &str = "m.call.invite";
const CANDIDATES: &str = "m.call.candidates";
const ANSWER: &str = "m.call.answer";
const SELECT_ANSWER: &str = "m.call.select_answer";
const REJECT: &str = "m.call.reject";
const HANGUP: &str = "m.call.hangup";

/// How long a call rings before it's given up, in milliseconds.
pub const RING_MS: u64 = 60_000;

/// The most candidates (ways to reach a side) taken from one event, and
/// the longest a session description may be: more is someone's mischief.
const MAX_CANDIDATES: usize = 40;
const MAX_SDP: usize = 20_000;

/// One way to reach a side of the call, as WebRTC writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_mline_index: Option<u16>,
}

/// What one side says to the other about a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signal {
    /// "I'm calling you": the caller's session description, and how long
    /// it rings (milliseconds).
    Invite { sdp: String, lifetime: u64 },
    /// More ways to reach the sender.
    Candidates(Vec<Candidate>),
    /// "I've picked up": the session description of the device that did.
    Answer { sdp: String },
    /// The caller, to the other side's devices: this one of you has it.
    SelectAnswer { party: String },
    /// "Not now", from the person called (all their devices stop ringing).
    Reject,
    /// The call is over (or the caller gave up).
    Hangup { reason: Option<String> },
}

/// A [`Signal`] and which call, and which device of the sender's, it's from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Said {
    pub call_id: String,
    pub party_id: String,
    pub signal: Signal,
}

pub fn is_call_event(kind: &str) -> bool {
    [INVITE, CANDIDATES, ANSWER, SELECT_ANSWER, REJECT, HANGUP].contains(&kind)
}

/// Reads a call event. `None` for anything else, or one that isn't made
/// as it should be (it's then as if it never came).
pub fn parse(kind: &str, content: &Value) -> Option<Said> {
    let text = |v: &Value, key: &str| v.get(key)?.as_str().map(str::to_owned);
    let call_id = text(content, "call_id").filter(|id| !id.is_empty() && id.len() <= 255)?;
    // Version 0 (very old apps) has no party id: the call id stands in.
    let party_id = text(content, "party_id").filter(|p| p.len() <= 255).unwrap_or_default();
    let sdp = |key: &str, kind: &str| {
        let d = content.get(key)?;
        (d.get("type")?.as_str()? == kind).then(|| text(d, "sdp")).flatten().filter(|s| s.len() <= MAX_SDP)
    };
    let signal = match kind {
        INVITE => Signal::Invite { sdp: sdp("offer", "offer")?, lifetime: content.get("lifetime").and_then(Value::as_u64).unwrap_or(RING_MS).min(RING_MS * 2) },
        ANSWER => Signal::Answer { sdp: sdp("answer", "answer")? },
        CANDIDATES => Signal::Candidates(
            content
                .get("candidates")?
                .as_array()?
                .iter()
                .take(MAX_CANDIDATES)
                .filter_map(|c| {
                    Some(Candidate {
                        candidate: text(c, "candidate").filter(|s| s.len() <= 1000)?,
                        sdp_mid: text(c, "sdpMid"),
                        sdp_mline_index: c.get("sdpMLineIndex").and_then(Value::as_u64).and_then(|n| u16::try_from(n).ok()),
                    })
                })
                .collect(),
        ),
        SELECT_ANSWER => Signal::SelectAnswer { party: text(content, "selected_party_id")? },
        REJECT => Signal::Reject,
        HANGUP => Signal::Hangup { reason: text(content, "reason").filter(|r| r.len() <= 64) },
        _ => return None,
    };
    Some(Said { call_id, party_id, signal })
}

/// The event that says `said`: its type and what's in it.
pub fn event(said: &Said) -> (&'static str, Value) {
    let mut content = json!({ "call_id": said.call_id, "party_id": said.party_id, "version": "1" });
    let kind = match &said.signal {
        Signal::Invite { sdp, lifetime } => {
            content["offer"] = json!({ "type": "offer", "sdp": sdp });
            content["lifetime"] = json!(lifetime);
            INVITE
        }
        Signal::Candidates(list) => {
            content["candidates"] = list
                .iter()
                .map(|c| json!({ "candidate": c.candidate, "sdpMid": c.sdp_mid, "sdpMLineIndex": c.sdp_mline_index }))
                .collect();
            CANDIDATES
        }
        Signal::Answer { sdp } => {
            content["answer"] = json!({ "type": "answer", "sdp": sdp });
            ANSWER
        }
        Signal::SelectAnswer { party } => {
            content["selected_party_id"] = json!(party);
            SELECT_ANSWER
        }
        Signal::Reject => REJECT,
        Signal::Hangup { reason } => {
            content["reason"] = json!(reason.as_deref().unwrap_or("user_hangup"));
            HANGUP
        }
    };
    (kind, content)
}

/// Says `said` in `room` (encrypted when the room is).
pub async fn send(room: &Room, said: &Said) -> Result<()> {
    let (kind, content) = event(said);
    room.send_raw(kind, content).await?;
    Ok(())
}

/// Whether `candidate` (an `a=candidate:` line, or what follows the `a=`)
/// goes through a relay: the only kind that doesn't give away an address
/// of this device or its network.
pub fn is_relay(candidate: &str) -> bool {
    let mut words = candidate.split_whitespace();
    // "... typ relay ...": the word after "typ".
    words.by_ref().any(|w| w == "typ") && words.next() == Some("relay")
}

/// `sdp` without the candidates that aren't a relay's (see [`is_relay`]).
/// What a relay candidate says of this device (`raddr`, `rport`) goes too:
/// the other side has no use for it.
pub fn relay_only(sdp: &str) -> String {
    let mut out = String::with_capacity(sdp.len());
    for line in sdp.split_inclusive('\n') {
        let text = line.trim_end();
        match text.strip_prefix("a=candidate:") {
            Some(c) if !is_relay(c) => {}
            Some(_) => {
                out.push_str(&hide_related(text));
                out.push_str(&line[text.len()..]);
            }
            None => out.push_str(line),
        }
    }
    out
}

/// A candidate without where it was asked from (`raddr x rport y`).
pub fn hide_related(candidate: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut words = candidate.split(' ');
    while let Some(w) = words.next() {
        match w {
            "raddr" => {
                words.next();
                out.extend(["raddr", "0.0.0.0"]);
            }
            "rport" => {
                words.next();
                out.extend(["rport", "0"]);
            }
            _ => out.push(w),
        }
    }
    out.join(" ")
}

/// The relay an account's server offers for calls, and the name and
/// password it gave for it (good for a while).
#[derive(Clone, Debug)]
pub struct Relay {
    pub uris: Vec<String>,
    pub username: String,
    pub password: String,
}

/// Asks `client`'s server for its relay. An error when it has none: then
/// there's no private way to call.
pub async fn relay(client: &Client) -> Result<Relay> {
    use matrix_sdk::ruma::api::client::voip::get_turn_server_info::v3::Request;
    let r = client.send(Request::new()).await?;
    // Only relays: a server may list plain address-finders (stun:) too,
    // which are for connecting directly.
    let uris: Vec<String> = r.uris.into_iter().filter(|u| u.starts_with("turn:") || u.starts_with("turns:")).collect();
    if uris.is_empty() {
        bail!("this account's server has no relay for calls");
    }
    Ok(Relay { uris, username: r.username, password: r.password })
}

/// A name for a new call, or for this device's side of one.
pub fn new_id() -> String {
    crate::vault::random_secret()[..24].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_and_reads_back_every_signal() {
        let candidate = Candidate { candidate: "candidate:1 1 udp 41885439 203.0.113.9 50000 typ relay raddr 0.0.0.0 rport 0".into(), sdp_mid: Some("0".into()), sdp_mline_index: Some(0) };
        for signal in [
            Signal::Invite { sdp: "v=0\r\n".into(), lifetime: RING_MS },
            Signal::Candidates(vec![candidate]),
            Signal::Answer { sdp: "v=0\r\n".into() },
            Signal::SelectAnswer { party: "DEVICE".into() },
            Signal::Reject,
            Signal::Hangup { reason: Some("user_hangup".into()) },
        ] {
            let said = Said { call_id: "c1".into(), party_id: "p1".into(), signal };
            let (kind, content) = event(&said);
            assert!(is_call_event(kind));
            assert_eq!(content["version"], "1");
            assert_eq!(parse(kind, &content), Some(said));
        }
    }

    #[test]
    fn badly_made_events_are_ignored() {
        // No call id, an answer where an offer should be, a huge description.
        assert_eq!(parse(INVITE, &json!({"offer": {"type": "offer", "sdp": "v=0"}})), None);
        assert_eq!(parse(INVITE, &json!({"call_id": "c", "offer": {"type": "answer", "sdp": "v=0"}})), None);
        assert_eq!(parse(INVITE, &json!({"call_id": "c", "offer": {"type": "offer", "sdp": "x".repeat(MAX_SDP + 1)}})), None);
        assert_eq!(parse("m.room.message", &json!({"call_id": "c"})), None);
        // A ring that would never end is cut short.
        let long = parse(INVITE, &json!({"call_id": "c", "lifetime": 86_400_000u64, "offer": {"type": "offer", "sdp": "v=0"}})).unwrap();
        assert_eq!(long.signal, Signal::Invite { sdp: "v=0".into(), lifetime: RING_MS * 2 });
    }

    #[test]
    fn only_the_relay_is_offered() {
        let sdp = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\n\
            a=candidate:1 1 udp 2130706431 192.168.1.20 40000 typ host\r\n\
            a=candidate:2 1 udp 1694498815 198.51.100.7 40000 typ srflx raddr 192.168.1.20 rport 40000\r\n\
            a=candidate:3 1 udp 41885439 203.0.113.9 50000 typ relay raddr 198.51.100.7 rport 40000\r\n\
            a=end-of-candidates\r\n";
        let out = relay_only(sdp);
        assert!(!out.contains("192.168.1.20") && !out.contains("198.51.100.7"), "{out}");
        assert!(out.contains("a=candidate:3 1 udp 41885439 203.0.113.9 50000 typ relay raddr 0.0.0.0 rport 0\r\n"), "{out}");
        assert!(out.starts_with("v=0\r\nm=audio") && out.ends_with("a=end-of-candidates\r\n"));
        assert!(is_relay("candidate:3 1 udp 1 203.0.113.9 5 typ relay"));
        assert!(!is_relay("candidate:1 1 udp 1 192.168.1.20 4 typ host"));
        // A relay named in a field that isn't the type doesn't count.
        assert!(!is_relay("candidate:relay 1 udp 1 192.168.1.20 4 typ host"));
    }
}
