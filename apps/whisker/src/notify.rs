//! Desktop notifications, through the freedesktop notification service
//! (every Linux desktop has one). Elsewhere they're not shown yet.

/// How much a notification says (Settings → Notifications).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    Off,
    /// Just "New message".
    Nothing,
    /// Who and where.
    Sender,
    /// Who, where and what.
    Text,
}

impl Detail {
    pub fn from_index(i: i32) -> Self {
        match i {
            0 => Self::Off,
            1 => Self::Nothing,
            3 => Self::Text,
            _ => Self::Sender,
        }
    }
}

/// Summary and body for a message notification.
pub fn content(detail: Detail, sender: &str, room: &str, direct: bool, text: &str) -> Option<(String, String)> {
    let short: String = text.lines().find(|l| !l.trim().is_empty()).unwrap_or_default().chars().take(200).collect();
    match detail {
        Detail::Off => None,
        Detail::Nothing => Some(("Whisker".into(), "New message".into())),
        Detail::Sender if direct => Some((sender.into(), "New message".into())),
        Detail::Sender => Some((room.into(), format!("New message from {sender}"))),
        Detail::Text if direct => Some((sender.into(), short)),
        Detail::Text => Some((room.into(), format!("{sender}: {short}"))),
    }
}

/// Escapes text for notification servers that read a little markup.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Shows (or, with the id a previous call returned, replaces) a notification.
#[cfg(target_os = "linux")]
pub fn show(summary: &str, body: &str, replaces: u32) -> Option<u32> {
    use std::collections::HashMap;
    use zbus::zvariant::Value;
    let conn = zbus::blocking::Connection::session().ok()?;
    let hints: HashMap<&str, Value> = HashMap::from([("category", Value::from("im.received")), ("desktop-entry", Value::from("whisker"))]);
    let reply = conn
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &("Whisker", replaces, "mail-message-new", escape(summary), escape(body), Vec::<&str>::new(), hints, -1i32),
        )
        .map_err(|e| tracing::debug!("notification failed: {e}"))
        .ok()?;
    reply.body().deserialize::<u32>().ok()
}

#[cfg(not(target_os = "linux"))]
pub fn show(_summary: &str, _body: &str, _replaces: u32) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_only_what_it_should() {
        assert_eq!(content(Detail::Off, "Bob", "Cats", false, "hi"), None);
        assert_eq!(content(Detail::Nothing, "Bob", "Cats", false, "secret").unwrap().1, "New message");
        assert_eq!(content(Detail::Sender, "Bob", "Cats", false, "secret").unwrap(), ("Cats".into(), "New message from Bob".into()));
        assert_eq!(content(Detail::Sender, "Bob", "Bob", true, "secret").unwrap(), ("Bob".into(), "New message".into()));
        assert_eq!(content(Detail::Text, "Bob", "Cats", false, "\nhello\nmore").unwrap().1, "Bob: hello");
        assert_eq!(escape("<b>&"), "&lt;b&gt;&amp;");
    }
}
