//! Polls (MSC3381): a question with answers, votes as events that point at
//! it, and an end. Read in both the stable (`m.poll.*`) and the unstable
//! (`org.matrix.msc3381.poll.*`) forms, which Element still sends; sent in
//! the unstable form with the stable fields beside it, so every app reads it.

use anyhow::{Result, bail};
use matrix_sdk::Room;
use matrix_sdk::ruma::{MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId};
use serde_json::{Value, json};

pub const START: &[&str] = &["m.poll.start", "org.matrix.msc3381.poll.start"];
pub const RESPONSE: &[&str] = &["m.poll.response", "org.matrix.msc3381.poll.response"];
pub const END: &[&str] = &["m.poll.end", "org.matrix.msc3381.poll.end"];

/// Most answers a poll may have (as Element allows).
pub const MAX_ANSWERS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Poll {
    pub question: String,
    /// (id, text), in order.
    pub answers: Vec<(String, String)>,
    /// Results shown while it's open (otherwise only once it's ended).
    pub disclosed: bool,
    /// How many answers one person may pick.
    pub max_selections: usize,
}

/// A vote, or the poll's end (`end`), pointing at the poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vote {
    pub event_id: OwnedEventId,
    pub poll: OwnedEventId,
    pub sender: OwnedUserId,
    pub ts: MilliSecondsSinceUnixEpoch,
    /// The answers picked (none: the vote taken back).
    pub answers: Vec<String>,
    pub end: bool,
}

/// Text from an extensible-events text field: a plain string (unstable
/// `org.matrix.msc1767.text`) or `m.text`'s list of `{body, mimetype}`.
fn text_of(v: &Value) -> Option<String> {
    if let Some(s) = v.get("org.matrix.msc1767.text").and_then(Value::as_str) {
        return Some(s.to_owned());
    }
    let list = v.get("m.text").or_else(|| v.get("org.matrix.msc1767.text"))?.as_array()?;
    let plain = list.iter().find(|t| t.get("mimetype").and_then(Value::as_str).is_none_or(|m| m == "text/plain")).or_else(|| list.first())?;
    plain.get("body").and_then(Value::as_str).map(str::to_owned)
}

/// The poll in a start event's `content` of type `kind`.
pub fn parse_start(kind: &str, content: &Value) -> Option<Poll> {
    if !START.contains(&kind) {
        return None;
    }
    let p = content.get("m.poll").or_else(|| content.get("org.matrix.msc3381.poll.start"))?;
    let question = text_of(p.get("question")?)?;
    let answers: Vec<(String, String)> = p
        .get("answers")?
        .as_array()?
        .iter()
        .filter_map(|a| {
            let id = a.get("m.id").or_else(|| a.get("id"))?.as_str()?.to_owned();
            Some((id, text_of(a)?))
        })
        .take(MAX_ANSWERS)
        .collect();
    if answers.is_empty() {
        return None;
    }
    let kind = p.get("kind").and_then(Value::as_str).unwrap_or("");
    let max = p.get("max_selections").and_then(Value::as_u64).unwrap_or(1).clamp(1, answers.len() as u64) as usize;
    Some(Poll { question, answers, disclosed: !kind.ends_with("undisclosed"), max_selections: max })
}

/// A vote or end event, from its type, `content` and where it came from.
pub fn parse_vote(kind: &str, content: &Value, event_id: OwnedEventId, sender: OwnedUserId, ts: MilliSecondsSinceUnixEpoch) -> Option<Vote> {
    let end = END.contains(&kind);
    if !end && !RESPONSE.contains(&kind) {
        return None;
    }
    let rel = content.get("m.relates_to")?;
    if rel.get("rel_type").and_then(Value::as_str) != Some("m.reference") {
        return None;
    }
    let poll = OwnedEventId::try_from(rel.get("event_id")?.as_str()?).ok()?;
    let answers = if end {
        Vec::new()
    } else {
        content
            .get("m.selections")
            .or_else(|| content.get("org.matrix.msc3381.poll.response")?.get("answers"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    };
    Some(Vote { event_id, poll, sender, ts, answers, end })
}

/// Whether events of type `kind` are poll events (shown as a poll, or
/// counted into one, not as hidden events).
pub fn is_poll_event(kind: &str) -> bool {
    START.contains(&kind) || RESPONSE.contains(&kind) || END.contains(&kind)
}

/// The count: per answer, how many picked it, and which ones `me` picked;
/// whether it ended (and when); how many voted. Each person's newest vote
/// before the end counts, only answers the poll has, at most
/// `max_selections` of them. Only the poll's sender can end it.
pub struct Tally {
    pub counts: Vec<usize>,
    pub mine: Vec<bool>,
    pub voters: usize,
    pub ended: bool,
}

pub fn tally(poll: &Poll, creator: &OwnedUserId, votes: &[&Vote], me: Option<&OwnedUserId>) -> Tally {
    let end = votes.iter().filter(|v| v.end && &v.sender == creator).map(|v| v.ts).min();
    let mut latest: std::collections::HashMap<&OwnedUserId, &Vote> = std::collections::HashMap::new();
    for v in votes.iter().filter(|v| !v.end && end.is_none_or(|e| v.ts <= e)) {
        let keep = latest.get(&v.sender).is_none_or(|old| (old.ts, &old.event_id) < (v.ts, &v.event_id));
        if keep {
            latest.insert(&v.sender, v);
        }
    }
    let mut counts = vec![0; poll.answers.len()];
    let mut mine = vec![false; poll.answers.len()];
    let mut voters = 0;
    for (who, v) in latest {
        let picked: Vec<usize> = v.answers.iter().filter_map(|a| poll.answers.iter().position(|(id, _)| id == a)).take(poll.max_selections).collect();
        if picked.is_empty() {
            continue;
        }
        voters += 1;
        for i in picked {
            counts[i] += 1;
            if Some(who) == me {
                mine[i] = true;
            }
        }
    }
    Tally { counts, mine, voters, ended: end.is_some() }
}

/// The text other apps show for a poll they can't draw.
fn fallback(question: &str, answers: &[String]) -> String {
    let mut s = question.to_owned();
    for (i, a) in answers.iter().enumerate() {
        s.push_str(&format!("\n{}. {a}", i + 1));
    }
    s
}

/// Starts a poll in `room`.
pub async fn send_poll(room: &Room, question: &str, answers: &[String], disclosed: bool, max_selections: usize) -> Result<OwnedEventId> {
    let question = question.trim();
    let answers: Vec<String> = answers.iter().map(|a| a.trim().to_owned()).filter(|a| !a.is_empty()).collect();
    if question.is_empty() {
        bail!("the poll needs a question");
    }
    if answers.len() < 2 {
        bail!("a poll needs at least two answers");
    }
    if answers.len() > MAX_ANSWERS {
        bail!("a poll can have at most {MAX_ANSWERS} answers");
    }
    let ids: Vec<String> = (0..answers.len()).map(|i| format!("{}-{i}", &crate::vault::random_secret()[..8])).collect();
    let max = max_selections.clamp(1, answers.len());
    let (kind_u, kind_s) = if disclosed { ("org.matrix.msc3381.poll.disclosed", "m.disclosed") } else { ("org.matrix.msc3381.poll.undisclosed", "m.undisclosed") };
    let text = fallback(question, &answers);
    let content = json!({
        "org.matrix.msc3381.poll.start": {
            "question": {"org.matrix.msc1767.text": question},
            "kind": kind_u,
            "max_selections": max,
            "answers": ids.iter().zip(&answers).map(|(id, a)| json!({"id": id, "org.matrix.msc1767.text": a})).collect::<Vec<_>>(),
        },
        "org.matrix.msc1767.text": text,
        "m.poll": {
            "question": {"m.text": [{"body": question}]},
            "kind": kind_s,
            "max_selections": max,
            "answers": ids.iter().zip(&answers).map(|(id, a)| json!({"m.id": id, "m.text": [{"body": a}]})).collect::<Vec<_>>(),
        },
        "m.text": [{"body": text}],
    });
    let res = room.send_raw("org.matrix.msc3381.poll.start", content).with_request_config(crate::timeline::send_config()).await?;
    Ok(res.response.event_id)
}

/// Votes `answers` (none: takes the vote back) in the poll `poll`.
pub async fn vote(room: &Room, poll: &OwnedEventId, answers: &[String]) -> Result<OwnedEventId> {
    let content = json!({
        "m.relates_to": {"rel_type": "m.reference", "event_id": poll},
        "org.matrix.msc3381.poll.response": {"answers": answers},
        "m.selections": answers,
    });
    let res = room.send_raw("org.matrix.msc3381.poll.response", content).with_request_config(crate::timeline::send_config()).await?;
    Ok(res.response.event_id)
}

/// Ends the poll `poll` (only its sender's end counts).
pub async fn end(room: &Room, poll: &OwnedEventId) -> Result<OwnedEventId> {
    let text = "The poll has ended.";
    let content = json!({
        "m.relates_to": {"rel_type": "m.reference", "event_id": poll},
        "org.matrix.msc3381.poll.end": {},
        "org.matrix.msc1767.text": text,
        "m.text": [{"body": text}],
    });
    let res = room.send_raw("org.matrix.msc3381.poll.end", content).with_request_config(crate::timeline::send_config()).await?;
    Ok(res.response.event_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> OwnedEventId {
        OwnedEventId::try_from(s).unwrap()
    }
    fn user(s: &str) -> OwnedUserId {
        OwnedUserId::try_from(s).unwrap()
    }
    fn ts(n: u64) -> MilliSecondsSinceUnixEpoch {
        MilliSecondsSinceUnixEpoch(matrix_sdk::ruma::UInt::new(n).unwrap())
    }

    #[test]
    fn reads_both_forms() {
        // As Element sends it.
        let unstable = json!({"org.matrix.msc3381.poll.start": {
            "question": {"org.matrix.msc1767.text": "Lunch?"},
            "kind": "org.matrix.msc3381.poll.undisclosed", "max_selections": 1,
            "answers": [{"id": "a", "org.matrix.msc1767.text": "Pizza"}, {"id": "b", "org.matrix.msc1767.text": "Soup"}]}});
        let p = parse_start("org.matrix.msc3381.poll.start", &unstable).unwrap();
        assert_eq!(p.question, "Lunch?");
        assert_eq!(p.answers, [("a".into(), "Pizza".into()), ("b".into(), "Soup".into())]);
        assert!(!p.disclosed);
        let stable = json!({"m.poll": {"question": {"m.text": [{"body": "Tea?"}]}, "kind": "m.disclosed", "max_selections": 5,
            "answers": [{"m.id": "y", "m.text": [{"body": "Yes"}]}, {"m.id": "n", "m.text": [{"mimetype": "text/html", "body": "<b>No</b>"}, {"body": "No"}]}]}});
        let p = parse_start("m.poll.start", &stable).unwrap();
        assert_eq!(p.answers[1].1, "No");
        assert!(p.disclosed);
        assert_eq!(p.max_selections, 2);
        assert!(parse_start("m.room.message", &stable).is_none());

        let v = parse_vote("org.matrix.msc3381.poll.response", &json!({"m.relates_to": {"rel_type": "m.reference", "event_id": "$p"}, "org.matrix.msc3381.poll.response": {"answers": ["a"]}}), id("$v"), user("@a:x"), ts(1)).unwrap();
        assert_eq!(v.answers, ["a"]);
        let v = parse_vote("m.poll.response", &json!({"m.relates_to": {"rel_type": "m.reference", "event_id": "$p"}, "m.selections": ["b"]}), id("$v"), user("@a:x"), ts(1)).unwrap();
        assert_eq!(v.answers, ["b"]);
        assert!(parse_vote("m.poll.end", &json!({"m.relates_to": {"rel_type": "m.reference", "event_id": "$p"}}), id("$e"), user("@a:x"), ts(1)).unwrap().end);
    }

    #[test]
    fn counts_votes() {
        let poll = Poll { question: "q".into(), answers: vec![("a".into(), "A".into()), ("b".into(), "B".into())], disclosed: true, max_selections: 1 };
        let (alice, bob, carol) = (user("@alice:x"), user("@bob:x"), user("@carol:x"));
        let v = |e: &str, who: &OwnedUserId, t: u64, a: &[&str], end: bool| Vote { event_id: id(e), poll: id("$p"), sender: who.clone(), ts: ts(t), answers: a.iter().map(|s| s.to_string()).collect(), end };
        let votes = [
            v("$1", &bob, 1, &["a"], false),
            v("$2", &bob, 2, &["b"], false),         // changed their mind
            v("$3", &carol, 3, &["a", "b"], false),  // one answer only
            v("$4", &bob, 4, &[], true),             // not theirs to end
            v("$5", &alice, 5, &["zzz"], false),     // not an answer
        ];
        let refs: Vec<&Vote> = votes.iter().collect();
        let t = tally(&poll, &alice, &refs, Some(&bob));
        assert_eq!(t.counts, [1, 1]);
        assert_eq!(t.mine, [false, true]);
        assert_eq!(t.voters, 2);
        assert!(!t.ended);
        // Ended by its sender: later votes don't count.
        let mut more = votes.to_vec();
        more.push(v("$6", &alice, 6, &[], true));
        more.push(v("$7", &carol, 7, &["b"], false));
        let refs: Vec<&Vote> = more.iter().collect();
        let t = tally(&poll, &alice, &refs, None);
        assert!(t.ended);
        assert_eq!(t.counts, [1, 1]);
    }
}
