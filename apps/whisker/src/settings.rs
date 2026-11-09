//! App preferences that are needed before any vault is unlocked (theme,
//! auto-lock). Stored in plain JSON because they say nothing about who you
//! are or which accounts you have; everything else lives in the vault.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    Dark,
    Light,
    System,
}

impl Theme {
    pub fn index(self) -> i32 {
        match self {
            Self::Dark => 0,
            Self::Light => 1,
            Self::System => 2,
        }
    }

    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Self::Light,
            2 => Self::System,
            _ => Self::Dark,
        }
    }
}

/// Choices offered by the auto-lock dropdown, in minutes (0 = never).
pub const AUTO_LOCK_MINUTES: [u64; 4] = [0, 5, 15, 60];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default)]
    pub theme: Theme,
    #[serde(default)]
    pub auto_lock_minutes: u64,
    /// Show bold/italic/clickable links instead of drag-selectable text.
    #[serde(default)]
    pub rich_text: bool,
    /// Show joins, leaves, renames and other room events in the timeline.
    #[serde(default)]
    pub show_events: bool,
    /// Notifications: 0 off, 1 just "New message", 2 sender and room, 3 with text.
    #[serde(default = "two")]
    pub notify_level: i32,
    /// Tell others when you're typing (off: it's metadata the server sees).
    #[serde(default)]
    pub send_typing: bool,
    /// Fix common typos in the composer as you type.
    #[serde(default = "yes")]
    pub autocorrect: bool,
    /// Red lines under misspelled words in the composer, with suggestions
    /// on right-click.
    #[serde(default = "yes")]
    pub spell_check: bool,
    /// Message menu entries turned off (see `Prefs.menu-*` in the UI).
    #[serde(default)]
    pub menu_hidden: std::collections::BTreeSet<String>,
    /// What to send to clear a room's messages (moderators): it opens the
    /// confirmation instead of being posted.
    #[serde(default = "clear_command")]
    pub clear_command: String,
    /// When sending a video's link, upload a preview frame made from it,
    /// so other apps show a picture (the video itself isn't uploaded).
    #[serde(default = "yes")]
    pub link_posters: bool,
    /// When your server can't get a file from another server, ask that
    /// server directly. Off: it would see your IP (unless you use a proxy).
    #[serde(default)]
    pub ask_file_servers: bool,
    /// When nobody else can give a file (its server shut down), ask a few
    /// other servers in its room for their copy. Off: they'd see your IP.
    #[serde(default)]
    pub ask_room_servers: bool,
    /// How files are prepared before upload.
    #[serde(default)]
    pub upload: whisker_core::upload::UploadOptions,
}

fn clear_command() -> String {
    "/clear".into()
}

fn yes() -> bool {
    true
}

fn two() -> i32 {
    2
}

impl Default for AppSettings {
    fn default() -> Self {
        serde_json::from_str("{}").expect("defaults")
    }
}

impl AppSettings {
    fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("settings.json")
    }

    pub fn load(data_dir: &Path) -> Self {
        std::fs::read(Self::path(data_dir))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, data_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        std::fs::write(Self::path(data_dir), serde_json::to_vec_pretty(self)?)
    }

    pub fn auto_lock_index(&self) -> i32 {
        AUTO_LOCK_MINUTES.iter().position(|m| *m == self.auto_lock_minutes).unwrap_or(0) as i32
    }
}

/// Shown on the settings page. Kept honest: say what is *not* protected too.
pub const ENCRYPTION_SUMMARY: &str = "\
On this device
• Encrypted: the vault (logins, access tokens, tags, nicknames, proxies, AI settings and API keys), \
using Argon2id and XChaCha20-Poly1305. Each account's message store and encryption keys are also \
encrypted, with a random key kept in the vault.
• Not encrypted: settings.json (theme, auto-lock and other display and privacy choices). The data folder also shows how many vault \
files and account folders exist, so a hidden vault hides who your alts are, not that you have some.
• While unlocked, messages are in memory in readable form.

Between you and the server
• Every connection uses TLS. With a proxy (e.g. Tor) the server doesn't see your IP address.
• Rooms that have encryption turned on are end-to-end encrypted, so the server can't read them. \
Rooms without it (most public rooms) can be read by every server in them.
• Servers always see metadata: which rooms you're in, who you talk to and when, display names \
and avatars.
• Files come through your own server. Only if you turn on “Ask a file's own server” does Whisker \
contact other servers directly, and then they see your IP address unless you use a proxy.
• Whisker doesn't send public read receipts, and sends typing notifications only if you turn them on.
• Devices aren't verified yet, so other people see your Whisker login as an unverified device.

AI accounts
• An AI account reads messages like any member, including in encrypted rooms it's been invited to, \
and sends the recent conversation to whatever AI API you connected it to (over TLS if the address is \
https). Use a local server (Ollama, llama.cpp, LM Studio) to keep it on your own machine.";
