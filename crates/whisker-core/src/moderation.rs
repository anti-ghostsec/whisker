//! Room admin controls, shared by the UI's slash commands and the AI agent.
//!
//! The homeserver enforces Matrix power levels whatever we do; this module
//! is just the one place every moderation action goes through.

use anyhow::{Result, anyhow, bail};
use matrix_sdk::Room;
use matrix_sdk::ruma::{Int, OwnedEventId, OwnedUserId};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ModAction {
    Kick { user: OwnedUserId, reason: Option<String> },
    Ban { user: OwnedUserId, reason: Option<String> },
    Unban { user: OwnedUserId, reason: Option<String> },
    Invite { user: OwnedUserId },
    Redact { event_id: OwnedEventId, reason: Option<String> },
    SetPowerLevel { user: OwnedUserId, level: i64 },
}

impl ModAction {
    /// A short machine name, matching the agent permission names.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Kick { .. } => "kick",
            Self::Ban { .. } => "ban",
            Self::Unban { .. } => "unban",
            Self::Invite { .. } => "invite",
            Self::Redact { .. } => "redact",
            Self::SetPowerLevel { .. } => "set_power_level",
        }
    }

    /// Parses a slash command: `/kick @u:hs [reason]`, `/ban`, `/unban`,
    /// `/invite @u:hs`, `/redact $event [reason]`, `/op @u:hs 50`, `/deop @u:hs`.
    /// Returns `None` if the text isn't a moderation command at all.
    pub fn parse_command(text: &str) -> Option<Result<Self>> {
        let text = text.trim();
        let rest = text.strip_prefix('/')?;
        let (cmd, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let args = args.trim();
        let (first, tail) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
        let reason = (!tail.trim().is_empty()).then(|| tail.trim().to_owned());
        let user = || -> Result<OwnedUserId> {
            first.try_into().map_err(|_| anyhow!("expected a user id like @name:server, got {first:?}"))
        };
        Some((|| {
            Ok(match cmd {
                "kick" => Self::Kick { user: user()?, reason },
                "ban" => Self::Ban { user: user()?, reason },
                "unban" => Self::Unban { user: user()?, reason },
                "invite" => Self::Invite { user: user()? },
                "redact" => Self::Redact {
                    event_id: first.try_into().map_err(|_| anyhow!("expected an event id like $abc"))?,
                    reason,
                },
                "op" => Self::SetPowerLevel {
                    user: user()?,
                    level: tail.trim().parse().unwrap_or(50),
                },
                "deop" => Self::SetPowerLevel { user: user()?, level: 0 },
                _ => return Err(anyhow!("not a moderation command")),
            })
        })())
        .filter(|r| !matches!(r, Err(e) if e.to_string() == "not a moderation command"))
    }

    pub async fn apply(&self, room: &Room) -> Result<()> {
        match self {
            Self::Kick { user, reason } => room.kick_user(user, reason.as_deref()).await?,
            Self::Ban { user, reason } => room.ban_user(user, reason.as_deref()).await?,
            Self::Unban { user, reason } => room.unban_user(user, reason.as_deref()).await?,
            Self::Invite { user } => room.invite_user_by_id(user).await?,
            Self::Redact { event_id, reason } => {
                room.redact(event_id, reason.as_deref(), None).await?;
            }
            Self::SetPowerLevel { user, level } => {
                let Ok(level) = Int::try_from(*level) else { bail!("power level out of range") };
                room.update_power_levels(vec![(user, level)]).await?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for ModAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let why = |r: &Option<String>| r.as_ref().map(|r| format!(" ({r})")).unwrap_or_default();
        match self {
            Self::Kick { user, reason } => write!(f, "kicked {user}{}", why(reason)),
            Self::Ban { user, reason } => write!(f, "banned {user}{}", why(reason)),
            Self::Unban { user, reason } => write!(f, "unbanned {user}{}", why(reason)),
            Self::Invite { user } => write!(f, "invited {user}"),
            Self::Redact { event_id, reason } => write!(f, "removed message {event_id}{}", why(reason)),
            Self::SetPowerLevel { user, level } => write!(f, "set {user}'s power level to {level}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        let a = ModAction::parse_command("/ban @spam:anoncat.org posting links").unwrap().unwrap();
        assert_eq!(
            a,
            ModAction::Ban { user: "@spam:anoncat.org".try_into().unwrap(), reason: Some("posting links".into()) }
        );
        let a = ModAction::parse_command("/op @mod:anoncat.org 75").unwrap().unwrap();
        assert_eq!(a, ModAction::SetPowerLevel { user: "@mod:anoncat.org".try_into().unwrap(), level: 75 });
        assert!(ModAction::parse_command("/kick notauser").unwrap().is_err());
        assert!(ModAction::parse_command("/me waves").is_none());
        assert!(ModAction::parse_command("hello").is_none());
    }
}
