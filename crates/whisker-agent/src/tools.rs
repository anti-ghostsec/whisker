//! Moderation tools offered to the model, and their checked execution.

use crate::llm::{ToolCall, ToolDef};
use crate::permissions::{Permission, Permissions};
use anyhow::{Result, anyhow, bail};
use matrix_sdk::Room;
use matrix_sdk::ruma::{OwnedUserId, UserId};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use whisker_core::ModAction;

fn user_reason(what: &'static str) -> Value {
    json!({
        "type": "object",
        "properties": {
            "user": { "type": "string", "description": format!("Full Matrix user id to {what}, like @name:server") },
            "reason": { "type": "string", "description": "Short reason shown to the room. Empty string for none." }
        },
        "required": ["user", "reason"],
        "additionalProperties": false
    })
}

/// Tool definitions for the moderation permissions in `perms`.
pub fn definitions(perms: &BTreeSet<Permission>) -> Vec<ToolDef> {
    perms
        .iter()
        .filter_map(|p| {
            Some(match p {
                Permission::Reply => return None,
                Permission::Kick => ToolDef {
                    name: "kick",
                    description: "Remove a user from this room. They can rejoin.",
                    schema: user_reason("kick"),
                },
                Permission::Ban => ToolDef {
                    name: "ban",
                    description: "Ban a user from this room so they cannot rejoin. Only for clear abuse or when an authorized moderator asks.",
                    schema: user_reason("ban"),
                },
                Permission::Unban => ToolDef {
                    name: "unban",
                    description: "Lift a ban on a user in this room.",
                    schema: user_reason("unban"),
                },
                Permission::Invite => ToolDef {
                    name: "invite",
                    description: "Invite a user to this room.",
                    schema: json!({
                        "type": "object",
                        "properties": { "user": { "type": "string", "description": "Full Matrix user id, like @name:server" } },
                        "required": ["user"],
                        "additionalProperties": false
                    }),
                },
                Permission::Redact => ToolDef {
                    name: "redact",
                    description: "Delete a message from this room by its event id (shown in brackets in the transcript, starting with $).",
                    schema: json!({
                        "type": "object",
                        "properties": {
                            "event_id": { "type": "string", "description": "Event id, like $abc123" },
                            "reason": { "type": "string", "description": "Short reason. Empty string for none." }
                        },
                        "required": ["event_id", "reason"],
                        "additionalProperties": false
                    }),
                },
                Permission::SetPowerLevel => ToolDef {
                    name: "set_power_level",
                    description: "Change a user's power level in this room (0 = normal user, 50 = moderator).",
                    schema: json!({
                        "type": "object",
                        "properties": {
                            "user": { "type": "string", "description": "Full Matrix user id" },
                            "level": { "type": "integer", "description": "New power level" }
                        },
                        "required": ["user", "level"],
                        "additionalProperties": false
                    }),
                },
            })
        })
        .collect()
}

/// Everything the safety checks need, without a live room (so it's testable).
pub struct Gate<'a> {
    pub me: &'a UserId,
    pub my_level: i64,
    /// Who asked, and the power level they act with: their room level (a
    /// named commander counts as at least the commander level), or the
    /// agent's own in autonomous mode for someone who isn't a commander.
    /// The agent never does to someone what the requester couldn't.
    pub requester: &'a UserId,
    pub authority: i64,
    pub allowed: &'a BTreeSet<Permission>,
    pub permissions: &'a Permissions,
    /// Your own accounts: never targeted.
    pub own_accounts: &'a BTreeSet<OwnedUserId>,
}

/// Turns a tool call into a [`ModAction`], re-checking every gate.
pub fn to_action(call: &ToolCall, cx: &Gate<'_>) -> Result<ModAction> {
    let perm = Permission::parse(&call.name).ok_or_else(|| anyhow!("unknown tool {}", call.name))?;
    if !perm.is_moderation() || !cx.allowed.contains(&perm) {
        bail!("not permitted: {}", call.name);
    }
    let mut v = call.input.clone();
    let Some(obj) = v.as_object_mut() else { bail!("tool input must be an object") };
    if obj.get("reason").and_then(Value::as_str).is_some_and(|r| r.trim().is_empty()) {
        obj.remove("reason");
    }
    obj.insert("action".into(), json!(perm.name()));
    let action: ModAction = serde_json::from_value(v).map_err(|e| anyhow!("bad arguments: {e}"))?;

    if let ModAction::SetPowerLevel { level, .. } = action {
        if level > cx.permissions.max_grant_level {
            bail!("may not grant above power level {}", cx.permissions.max_grant_level);
        }
        if level >= cx.my_level {
            bail!("may not grant a level at or above its own ({})", cx.my_level);
        }
        if level >= cx.authority {
            bail!("may not grant a level at or above the requester's ({})", cx.authority);
        }
    }
    Ok(action)
}

/// Who the action is aimed at, as the room has them: the user (or, for a
/// deletion, the message's sender), their power level, and whether they
/// are in the room (joined, invited or knocking).
#[derive(Debug, Clone)]
pub struct Target {
    pub user: OwnedUserId,
    pub level: i64,
    pub member: bool,
}

/// The answer for every target the agent won't touch. The same words for
/// a protected account as for someone who isn't here, so asking can't
/// tell which ids are the owner's.
const REFUSED: &str = "can't do that to that user here";

/// Checks the action against who it's aimed at.
pub fn check_target(action: &ModAction, t: &Target, cx: &Gate<'_>) -> Result<()> {
    if matches!(action, ModAction::Invite { .. } | ModAction::Unban { .. }) {
        return Ok(());
    }
    // People's own messages are theirs to have deleted.
    let own_message = matches!(action, ModAction::Redact { .. }) && t.user == cx.requester;
    if own_message {
        return Ok(());
    }
    if t.user == cx.me || cx.permissions.protected.contains(&t.user) || cx.own_accounts.contains(&t.user) || !t.member {
        bail!(REFUSED);
    }
    if t.user != cx.requester && t.level >= cx.authority {
        bail!("{} has a power level of {} or more, the same as or above the requester's; refusing", t.user, cx.authority);
    }
    Ok(())
}

pub async fn execute(call: &ToolCall, room: &Room, gate: &Gate<'_>) -> Result<ModAction> {
    let action = to_action(call, gate)?;
    let user = match &action {
        ModAction::Kick { user, .. }
        | ModAction::Ban { user, .. }
        | ModAction::Unban { user, .. }
        | ModAction::Invite { user }
        | ModAction::SetPowerLevel { user, .. } => Some(user.clone()),
        ModAction::Redact { event_id, .. } => {
            let ev = room.event(event_id, None).await.map_err(|_| anyhow!("no such message here"))?;
            Some(ev.raw().get_field::<OwnedUserId>("sender").ok().flatten().ok_or_else(|| anyhow!("no such message here"))?)
        }
    };
    if let Some(user) = user {
        use matrix_sdk::ruma::events::room::member::MembershipState;
        let member = matches!(
            room.get_member_no_sync(&user).await.ok().flatten().map(|m| m.membership().clone()),
            Some(MembershipState::Join | MembershipState::Invite | MembershipState::Knock)
        );
        let level = match room.get_user_power_level(&user).await {
            Ok(matrix_sdk::ruma::events::room::power_levels::UserPowerLevel::Int(i)) => i.into(),
            Ok(_) => i64::MAX,
            Err(_) => 0,
        };
        check_target(&action, &Target { user, level, member }, gate)?;
    }
    action.apply(room).await?;
    Ok(action)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, input: Value) -> ToolCall {
        ToolCall { id: "t".into(), name: name.into(), input }
    }

    #[test]
    fn gates_hold() {
        let me: OwnedUserId = "@bot:anoncat.org".try_into().unwrap();
        let alt: OwnedUserId = "@myalt:other.org".try_into().unwrap();
        let permissions = Permissions { max_grant_level: 50, ..Default::default() };
        let allowed: BTreeSet<Permission> = [Permission::Ban, Permission::SetPowerLevel].into();
        let own: BTreeSet<OwnedUserId> = [alt.clone()].into();
        let boss: OwnedUserId = "@boss:x.org".try_into().unwrap();
        let g = Gate { me: &me, my_level: 75, requester: &boss, authority: 60, allowed: &allowed, permissions: &permissions, own_accounts: &own };

        let ok = to_action(&call("ban", json!({"user": "@spam:x.org", "reason": ""})), &g).unwrap();
        assert_eq!(ok, ModAction::Ban { user: "@spam:x.org".try_into().unwrap(), reason: None });

        assert!(to_action(&call("kick", json!({"user": "@spam:x.org", "reason": "x"})), &g).is_err(), "kick not granted");
        assert!(to_action(&call("set_power_level", json!({"user": "@a:x.org", "level": 100})), &g).is_err(), "above cap");
        assert!(to_action(&call("set_power_level", json!({"user": "@a:x.org", "level": 50})), &g).is_ok());
        assert!(to_action(&call("ban", json!({"user": "not-a-user", "reason": ""})), &g).is_err());

        let t = |u: &OwnedUserId, level: i64, member: bool| Target { user: u.clone(), level, member };
        let spam: OwnedUserId = "@spam:x.org".try_into().unwrap();
        // Protected accounts and people who aren't here get the same answer.
        let a = check_target(&ok, &t(&alt, 0, true), &g).unwrap_err().to_string();
        let b = check_target(&ok, &t(&spam, 0, false), &g).unwrap_err().to_string();
        assert_eq!(a, b);
        assert!(check_target(&ok, &t(&me, 0, true), &g).is_err(), "never itself");
        assert!(check_target(&ok, &t(&spam, 0, true), &g).is_ok());
        // Nobody at or above the requester's level.
        let mod_b: OwnedUserId = "@modb:x.org".try_into().unwrap();
        assert!(check_target(&ok, &t(&mod_b, 60, true), &g).is_err());
        assert!(check_target(&ok, &t(&mod_b, 50, true), &g).is_ok());
        // Nor a level at or above the requester's.
        assert!(to_action(&call("set_power_level", json!({"user": "@a:x.org", "level": 60})), &Gate { my_level: 100, ..g }).is_err());
        // Deleting: someone's own message is fine, a protected sender's isn't.
        let redact = ModAction::Redact { event_id: "$e".try_into().unwrap(), reason: None };
        assert!(check_target(&redact, &t(&boss, 60, true), &g).is_ok());
        assert!(check_target(&redact, &t(&alt, 0, true), &g).is_err());
    }
}
