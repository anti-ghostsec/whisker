//! What an agent may do, and on whose say-so.
//!
//! Two gates sit in front of every moderation action:
//! 1. **Permissions**: the action must be granted, globally or for that room.
//! 2. **Authority**: the message that woke the agent must come from a
//!    commander (or the agent must be set to moderate on its own).
//!
//! If either gate is closed the model is never even offered the tool, and
//! the executor checks both again before acting. The homeserver's power
//! levels are a third gate we don't control.

use matrix_sdk::ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Post messages. Without it the agent is read-only.
    Reply,
    Redact,
    Kick,
    Ban,
    Unban,
    Invite,
    SetPowerLevel,
}

impl Permission {
    pub const ALL: [Permission; 7] = [
        Self::Reply,
        Self::Redact,
        Self::Kick,
        Self::Ban,
        Self::Unban,
        Self::Invite,
        Self::SetPowerLevel,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::Redact => "redact",
            Self::Kick => "kick",
            Self::Ban => "ban",
            Self::Unban => "unban",
            Self::Invite => "invite",
            Self::SetPowerLevel => "set_power_level",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == s)
    }

    pub fn is_moderation(self) -> bool {
        self != Self::Reply
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Permissions {
    /// Granted in every room the agent is in.
    #[serde(default = "default_global")]
    pub global: BTreeSet<Permission>,
    /// Per-room grants. A room listed here uses this set *instead of* `global`.
    #[serde(default)]
    pub rooms: BTreeMap<OwnedRoomId, BTreeSet<Permission>>,
    /// Who can tell the agent to moderate.
    #[serde(default)]
    pub commanders: Commanders,
    /// Let the agent moderate on its own judgement (e.g. spam), without a
    /// commander asking. Off by default.
    #[serde(default)]
    pub autonomous: bool,
    /// The agent never acts against these users (your own accounts are
    /// added automatically).
    #[serde(default)]
    pub protected: BTreeSet<OwnedUserId>,
    /// Highest power level the agent may hand out with `set_power_level`.
    #[serde(default)]
    pub max_grant_level: i64,
}

fn default_global() -> BTreeSet<Permission> {
    [Permission::Reply].into()
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            global: default_global(),
            rooms: BTreeMap::new(),
            commanders: Commanders::default(),
            autonomous: false,
            protected: BTreeSet::new(),
            max_grant_level: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commanders {
    /// Room members at or above this power level count as commanders.
    #[serde(default = "default_min_level")]
    pub min_power_level: i64,
    /// These users count as commanders everywhere.
    #[serde(default)]
    pub users: BTreeSet<OwnedUserId>,
}

fn default_min_level() -> i64 {
    50
}

impl Default for Commanders {
    fn default() -> Self {
        Self { min_power_level: default_min_level(), users: BTreeSet::new() }
    }
}

impl Permissions {
    pub fn granted(&self, room: &RoomId) -> &BTreeSet<Permission> {
        self.rooms.get(room).unwrap_or(&self.global)
    }

    pub fn is_commander(&self, user: &UserId, power_level: i64) -> bool {
        self.commanders.users.contains(user) || power_level >= self.commanders.min_power_level
    }

    /// The permissions the agent can use right now, for a message from a
    /// sender with `sender_level` in `room`.
    pub fn effective(&self, room: &RoomId, sender: &UserId, sender_level: i64) -> BTreeSet<Permission> {
        let may_moderate = self.autonomous || self.is_commander(sender, sender_level);
        self.granted(room)
            .iter()
            .copied()
            .filter(|p| !p.is_moderation() || may_moderate)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(s: &str) -> OwnedUserId {
        s.try_into().unwrap()
    }

    #[test]
    fn non_commanders_never_unlock_moderation() {
        let room: OwnedRoomId = "!r:anoncat.org".try_into().unwrap();
        let mut p = Permissions::default();
        p.global.extend([Permission::Ban, Permission::Kick]);

        let random = p.effective(&room, &uid("@rando:x.org"), 0);
        assert_eq!(random, [Permission::Reply].into());

        let mod_ = p.effective(&room, &uid("@mod:x.org"), 50);
        assert!(mod_.contains(&Permission::Ban));

        p.commanders.users.insert(uid("@owner:x.org"));
        assert!(p.effective(&room, &uid("@owner:x.org"), 0).contains(&Permission::Kick));

        p.autonomous = true;
        assert!(p.effective(&room, &uid("@rando:x.org"), 0).contains(&Permission::Ban));
    }

    #[test]
    fn room_override_replaces_global() {
        let room: OwnedRoomId = "!r:anoncat.org".try_into().unwrap();
        let mut p = Permissions::default();
        p.global.insert(Permission::Ban);
        p.rooms.insert(room.clone(), [Permission::Reply, Permission::Redact].into());
        let e = p.effective(&room, &uid("@mod:x.org"), 100);
        assert!(e.contains(&Permission::Redact));
        assert!(!e.contains(&Permission::Ban));
    }
}
