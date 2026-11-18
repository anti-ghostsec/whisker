//! One-line descriptions of room events that aren't messages: joins,
//! leaves, kicks, name and topic changes and so on. Shown in the timeline
//! (greyed out) when the "show room events" setting is on.

use matrix_sdk::ruma::events::room::history_visibility::HistoryVisibility;
use matrix_sdk::ruma::events::room::join_rules::JoinRule;
use matrix_sdk::ruma::events::room::member::MembershipChange;
use matrix_sdk::ruma::events::{AnySyncStateEvent, SyncStateEvent};

/// What `ev` did, as a sentence, with `name(user_id)` giving display names.
/// `None` for events not worth a line (or redacted ones).
pub fn describe(ev: &AnySyncStateEvent, name: impl Fn(&str) -> String) -> Option<String> {
    let who = name(ev.sender().as_str());
    let text = match ev {
        AnySyncStateEvent::RoomMember(SyncStateEvent::Original(e)) => {
            let target = e.content.displayname.clone().unwrap_or_else(|| name(e.state_key.as_str()));
            let reason = e.content.reason.as_deref().filter(|r| !r.is_empty()).map(|r| format!(": {r}")).unwrap_or_default();
            match e.membership_change() {
                MembershipChange::Joined | MembershipChange::InvitationAccepted | MembershipChange::KnockAccepted => format!("{target} joined"),
                MembershipChange::Left => format!("{target} left{reason}"),
                MembershipChange::Banned | MembershipChange::KickedAndBanned => format!("{who} banned {target}{reason}"),
                MembershipChange::Unbanned => format!("{who} unbanned {target}"),
                MembershipChange::Kicked => format!("{who} removed {target}{reason}"),
                MembershipChange::Invited => format!("{who} invited {target}"),
                MembershipChange::InvitationRejected => format!("{target} declined the invitation"),
                MembershipChange::InvitationRevoked => format!("{who} withdrew {target}'s invitation"),
                MembershipChange::Knocked => format!("{target} asked to join{reason}"),
                MembershipChange::KnockRetracted => format!("{target} no longer wants to join"),
                MembershipChange::KnockDenied => format!("{who} turned down {target}'s request to join"),
                MembershipChange::ProfileChanged { displayname_change, avatar_url_change } => {
                    let old = displayname_change
                        .as_ref()
                        .and_then(|c| c.old)
                        .map(str::to_owned)
                        .unwrap_or_else(|| name(e.state_key.as_str()));
                    let renamed = displayname_change.is_some();
                    match (displayname_change.and_then(|c| c.new), avatar_url_change.is_some()) {
                        (Some(new), _) => format!("{old} is now {new}"),
                        (None, false) if renamed => format!("{old} removed their display name"),
                        (None, true) => format!("{target} changed their picture"),
                        _ => return None,
                    }
                }
                _ => return None,
            }
        }
        AnySyncStateEvent::RoomCreate(SyncStateEvent::Original(_)) => format!("{who} created the room"),
        AnySyncStateEvent::RoomName(SyncStateEvent::Original(e)) => {
            if e.content.name.is_empty() {
                format!("{who} removed the room name")
            } else {
                format!("{who} renamed the room to {}", e.content.name)
            }
        }
        AnySyncStateEvent::RoomTopic(SyncStateEvent::Original(e)) => {
            if e.content.topic.is_empty() {
                format!("{who} removed the description")
            } else {
                format!("{who} changed the description to: {}", e.content.topic)
            }
        }
        AnySyncStateEvent::RoomAvatar(SyncStateEvent::Original(_)) => format!("{who} changed the room picture"),
        AnySyncStateEvent::RoomEncryption(SyncStateEvent::Original(_)) => format!("{who} turned on end-to-end encryption"),
        AnySyncStateEvent::RoomPowerLevels(SyncStateEvent::Original(_)) => format!("{who} changed who can do what in the room"),
        AnySyncStateEvent::RoomCanonicalAlias(SyncStateEvent::Original(e)) => match &e.content.alias {
            Some(a) => format!("{who} set the room address to {a}"),
            None => format!("{who} removed the room address"),
        },
        AnySyncStateEvent::RoomPinnedEvents(SyncStateEvent::Original(_)) => format!("{who} changed the pinned messages"),
        AnySyncStateEvent::RoomTombstone(SyncStateEvent::Original(e)) => {
            format!("{who} upgraded the room; it continues in {}", e.content.replacement_room)
        }
        AnySyncStateEvent::RoomJoinRules(SyncStateEvent::Original(e)) => match e.content.join_rule {
            JoinRule::Public => format!("{who} made the room public"),
            JoinRule::Invite => format!("{who} made the room invite-only"),
            JoinRule::Knock | JoinRule::KnockRestricted(_) => format!("{who} let people ask to join"),
            JoinRule::Restricted(_) => format!("{who} limited joining to members of other rooms"),
            _ => format!("{who} changed who can join"),
        },
        AnySyncStateEvent::RoomHistoryVisibility(SyncStateEvent::Original(e)) => match e.content.history_visibility {
            HistoryVisibility::WorldReadable => format!("{who} made the history readable by anyone"),
            HistoryVisibility::Shared => format!("{who} made the full history visible to members"),
            HistoryVisibility::Invited => format!("{who} made the history visible to members from their invitation on"),
            HistoryVisibility::Joined => format!("{who} made the history visible to members from when they joined"),
            _ => format!("{who} changed who can read the history"),
        },
        AnySyncStateEvent::RoomServerAcl(SyncStateEvent::Original(_)) => format!("{who} changed which servers may take part"),
        AnySyncStateEvent::RoomGuestAccess(SyncStateEvent::Original(_)) => format!("{who} changed guest access"),
        _ => return None,
    };
    Some(text)
}

/// What a room event changed, one line per value, for developer mode:
/// `users › @a:x.org: 50 → 100`, `events_default: 0` (newly set),
/// `ban: 50 → removed`. Compares the event's `content` with the
/// `prev_content` the server sends along (in `unsigned`, or at the top on
/// older servers); nested settings are walked down to their values.
pub fn changes(raw: &str) -> Vec<String> {
    use serde_json::Value;
    use std::collections::{BTreeMap, BTreeSet};
    fn flatten<'a>(v: &'a Value, path: String, out: &mut BTreeMap<String, &'a Value>) {
        match v {
            Value::Object(m) if !m.is_empty() => {
                for (k, v) in m {
                    flatten(v, if path.is_empty() { k.clone() } else { format!("{path} › {k}") }, out);
                }
            }
            _ if path.is_empty() => {}
            _ => {
                out.insert(path, v);
            }
        }
    }
    fn short(v: &Value) -> String {
        let s = v.to_string();
        match s.char_indices().nth(120) {
            Some((i, _)) => format!("{}…", &s[..i]),
            None => s,
        }
    }
    let Ok(v) = serde_json::from_str::<Value>(raw) else { return Vec::new() };
    let empty = Value::Object(Default::default());
    let new = v.get("content").unwrap_or(&empty);
    let old = v.pointer("/unsigned/prev_content").or_else(|| v.get("prev_content")).unwrap_or(&empty);
    let (mut a, mut b) = (BTreeMap::new(), BTreeMap::new());
    flatten(old, String::new(), &mut a);
    flatten(new, String::new(), &mut b);
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    keys.into_iter()
        .filter_map(|k| match (a.get(k), b.get(k)) {
            (Some(o), Some(n)) if o == n => None,
            (Some(o), Some(n)) => Some(format!("{k}: {} → {}", short(o), short(n))),
            (None, Some(n)) => Some(format!("{k}: {}", short(n))),
            (Some(o), None) => Some(format!("{k}: {} → removed", short(o))),
            (None, None) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::timeline::state_event;
    use matrix_sdk::ruma::serde::Raw;

    fn describe_json(v: serde_json::Value) -> Option<String> {
        let raw: Raw<matrix_sdk::ruma::events::AnySyncTimelineEvent> = serde_json::from_value(v).unwrap();
        super::describe(&state_event(&raw).unwrap(), |id| id.trim_start_matches('@').split(':').next().unwrap().to_owned())
    }

    /// `prev_content` in `unsigned` (the spec) or at the top (older servers):
    /// either way a rename is a rename, not a join.
    #[test]
    fn renames_are_not_joins() {
        let ev = |unsigned: bool| {
            let mut v = serde_json::json!({
                "type": "m.room.member", "state_key": "@nully:x", "sender": "@nully:x", "event_id": "$e", "origin_server_ts": 1,
                "content": {"membership": "join", "displayname": "New"}
            });
            let prev = serde_json::json!({"membership": "join", "displayname": "Old"});
            if unsigned { v["unsigned"] = serde_json::json!({"prev_content": prev}); } else { v["prev_content"] = prev; }
            v
        };
        assert_eq!(describe_json(ev(true)).as_deref(), Some("Old is now New"));
        assert_eq!(describe_json(ev(false)).as_deref(), Some("Old is now New"));
        // Nothing changed at all: no line.
        let mut same = ev(true);
        same["unsigned"]["prev_content"]["displayname"] = "New".into();
        assert_eq!(describe_json(same), None);
    }

    #[test]
    fn changes_list_each_value() {
        let ev = serde_json::json!({
            "type": "m.room.power_levels", "state_key": "", "sender": "@a:x", "event_id": "$p", "origin_server_ts": 1,
            "content": {"events": {"m.reaction": 0, "m.room.name": 50}, "users": {"@a:x": 100, "@new:x": 15}, "ban": 50},
            "unsigned": {"prev_content": {"events": {"m.reaction": 10, "m.room.name": 50}, "users": {"@a:x": 100, "@old:x": 0}, "ban": 50, "kick": 50}}
        });
        assert_eq!(
            super::changes(&ev.to_string()),
            ["events › m.reaction: 10 → 0", "kick: 50 → removed", "users › @new:x: 15", "users › @old:x: 0 → removed"]
        );
        // No earlier state: everything it sets.
        let first = serde_json::json!({"type": "x.custom", "state_key": "", "content": {"icon": {"key": "🐱"}, "color": "#fc8c25"}});
        assert_eq!(super::changes(&first.to_string()), ["color: \"#fc8c25\"", "icon › key: \"🐱\""]);
    }
}
