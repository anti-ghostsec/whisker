//! Your sticker collection: pictures saved from chats under a short name,
//! sent by typing `:name:`. Kept in the vault (encrypted, like the rest of
//! your data) rather than in account data on the server, so the server
//! doesn't learn which pictures you keep. Saving one reuses the file
//! that's already on the server (no new upload), and an encrypted
//! picture stays encrypted: its key is kept with it.

use crate::timeline::Media;
use anyhow::{Result, bail};
use matrix_sdk::ruma::UInt;
use matrix_sdk::ruma::events::room::message::{ImageMessageEventContent, MessageType};
use matrix_sdk::ruma::events::room::{ImageInfo, MediaSource};
use serde::{Deserialize, Serialize};

/// Where the collection is kept in the vault.
pub const VAULT_KEY: &str = "stickers";

/// Where your custom emoji are kept (same shape as stickers). Unlike a
/// sticker, a custom emoji is always a plain (unencrypted) file: it's sent
/// as an inline picture in a message's text, or as a reaction, and apps
/// only read those as plain `mxc://` addresses.
pub const EMOJI_KEY: &str = "emoji";

/// Widest and tallest a sticker is shown, so other apps draw it small.
const SHOWN_PX: u64 = 256;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sticker {
    pub name: String,
    /// The file, as a message's `url` or `file` (with its key).
    pub source: serde_json::Value,
    pub mime: Option<String>,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub size: Option<u64>,
}

impl Sticker {
    /// A sticker from a picture in a chat.
    pub fn from_media(name: &str, m: &Media) -> Result<Self> {
        let Some(name) = clean_name(name) else { bail!("use letters, numbers, - or _ for the name") };
        Ok(Self { name, source: serde_json::to_value(&m.source)?, mime: m.mime.clone(), width: m.width, height: m.height, size: m.size })
    }

    /// Its plain file address, for a custom emoji (`None` if encrypted).
    pub fn mxc(&self) -> Option<matrix_sdk::ruma::OwnedMxcUri> {
        match self.source().ok()? {
            MediaSource::Plain(u) => Some(u),
            MediaSource::Encrypted(_) => None,
        }
    }

    pub fn source(&self) -> Result<MediaSource> {
        Ok(serde_json::from_value(self.source.clone())?)
    }

    /// The message that sends it: a picture, sized to show small.
    pub fn message(&self) -> Result<MessageType> {
        let mut content = ImageMessageEventContent::new(self.name.clone(), self.source()?);
        let mut info = ImageInfo::new();
        info.mimetype = self.mime.clone();
        info.size = self.size.and_then(|s| UInt::try_from(s).ok());
        if let (Some(w), Some(h)) = (self.width, self.height)
            && w > 0
            && h > 0
        {
            let scale = (SHOWN_PX as f64 / w as f64).min(SHOWN_PX as f64 / h as f64).min(1.0);
            info.width = UInt::try_from(((w as f64 * scale).round() as u64).max(1)).ok();
            info.height = UInt::try_from(((h as f64 * scale).round() as u64).max(1)).ok();
        }
        content.info = Some(Box::new(info));
        Ok(MessageType::Image(content))
    }
}

/// A name usable as `:name:` — lowercase letters, digits, `-` and `_`,
/// up to 32 — or `None`.
pub fn clean_name(name: &str) -> Option<String> {
    let n: String = name.trim().trim_matches(':').to_lowercase().chars().map(|c| if c.is_whitespace() { '_' } else { c }).collect();
    (!n.is_empty() && n.len() <= 32 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')).then_some(n)
}

/// The sticker a message is, when it's only `:name:`.
pub fn find<'a>(list: &'a [Sticker], text: &str) -> Option<&'a Sticker> {
    let name = text.trim().strip_prefix(':')?.strip_suffix(':')?;
    list.iter().find(|s| s.name == name)
}

/// Stickers whose names start with (then contain) `q`.
pub fn suggest<'a>(list: &'a [Sticker], q: &str) -> Vec<&'a Sticker> {
    let q = q.to_lowercase();
    let mut out: Vec<&Sticker> = list.iter().filter(|s| s.name.starts_with(&q)).collect();
    out.extend(list.iter().filter(|s| !s.name.starts_with(&q) && s.name.contains(&q)));
    out
}

/// `html` (a message's formatted text) with each `:name:` of one of
/// `emoji` (name, plain mxc address) as an inline emoji picture, the way
/// Matrix apps show custom emoji. Names inside tags (a link's address, say)
/// are left alone. `None` when there's none in it.
pub fn inline_emoji(html: &str, emoji: &[(String, String)]) -> Option<String> {
    if emoji.is_empty() || !html.contains(':') {
        return None;
    }
    let mut out = String::with_capacity(html.len());
    let mut any = false;
    let mut in_tag = false;
    let mut i = 0;
    let b = html.as_bytes();
    while i < html.len() {
        let c = b[i];
        if c == b'<' {
            in_tag = true;
        } else if c == b'>' {
            in_tag = false;
        } else if c == b':' && !in_tag
            && let Some(end) = html[i + 1..].find(':').map(|e| i + 1 + e)
        {
            let name = &html[i + 1..end];
            if let Some((_, mxc)) = emoji.iter().find(|(n, _)| n == name) {
                out.push_str(&format!(r#"<img data-mx-emoticon height="32" src="{mxc}" alt=":{name}:" title=":{name}:">"#));
                any = true;
                i = end + 1;
                continue;
            }
        }
        let ch = html[i..].chars().next().unwrap_or_default();
        out.push(ch);
        i += ch.len_utf8();
    }
    any.then_some(out)
}

/// Plain text as HTML (for a message with no formatting of its own).
pub fn plain_html(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\n', "<br>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::MediaKind;

    fn media(source: MediaSource) -> Media {
        Media { kind: MediaKind::Image, source, thumbnail: None, name: "x.png".into(), mime: Some("image/png".into()), size: Some(1000), width: Some(1024), height: Some(512), linked: false }
    }

    #[test]
    fn keeps_the_file_and_sends_it_small() {
        let s = Sticker::from_media("Big Cat", &media(MediaSource::Plain("mxc://x.org/cat".into()))).unwrap();
        assert_eq!(s.name, "big_cat");
        let back: Sticker = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        let MessageType::Image(c) = s.message().unwrap() else { panic!() };
        assert_eq!(c.body, "big_cat");
        assert!(matches!(c.source, MediaSource::Plain(ref u) if u.as_str() == "mxc://x.org/cat"));
        let info = c.info.unwrap();
        assert_eq!((info.width.map(u64::from), info.height.map(u64::from)), (Some(256), Some(128)));
    }

    #[test]
    fn keeps_an_encrypted_files_key() {
        let file: matrix_sdk::ruma::events::room::EncryptedFile = serde_json::from_value(serde_json::json!({
            "url": "mxc://x.org/secret", "v": "v2",
            "key": {"kty": "oct", "key_ops": ["encrypt", "decrypt"], "alg": "A256CTR", "k": "qcHVMSgYg-71CauWBezXI5qkaRb0LuIy-Wx5kIaHMIA", "ext": true},
            "iv": "X85+XgHN+HEAAAAAAAAAAA", "hashes": {"sha256": "5qG4fFnbbVdlAB1Q72JDKwCagV6Dbkx9uds4rSak37c"}
        }))
        .unwrap();
        let s = Sticker::from_media("secret", &media(MediaSource::Encrypted(Box::new(file)))).unwrap();
        let back: Sticker = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(matches!(back.source().unwrap(), MediaSource::Encrypted(f) if f.url.as_str() == "mxc://x.org/secret"));
    }

    #[test]
    fn inline_emoji_in_text() {
        let e = vec![("blob".to_owned(), "mxc://x.org/b".to_owned())];
        assert_eq!(
            inline_emoji("hi :blob: :nope: 10:30", &e).unwrap(),
            r#"hi <img data-mx-emoticon height="32" src="mxc://x.org/b" alt=":blob:" title=":blob:"> :nope: 10:30"#
        );
        // Not inside a tag, and nothing when there's none.
        assert_eq!(inline_emoji(r#"<a href="https://x.org/:blob:/">:blob:</a>"#, &e).unwrap().matches("<img").count(), 1);
        assert!(inline_emoji("plain", &e).is_none());
        assert_eq!(plain_html("a<b>\nc"), "a&lt;b&gt;<br>c");
        let s = Sticker::from_media("x", &media(MediaSource::Plain("mxc://x.org/a".into()))).unwrap();
        assert_eq!(s.mxc().unwrap().as_str(), "mxc://x.org/a");
    }

    #[test]
    fn names_and_lookups() {
        assert_eq!(clean_name(":LOL:").as_deref(), Some("lol"));
        assert_eq!(clean_name("no/slash"), None);
        assert_eq!(clean_name(""), None);
        let list: Vec<Sticker> = ["lol", "lolcat", "catlol"]
            .iter()
            .map(|n| Sticker::from_media(n, &media(MediaSource::Plain("mxc://x.org/a".into()))).unwrap())
            .collect();
        assert_eq!(find(&list, " :lolcat: ").map(|s| s.name.as_str()), Some("lolcat"));
        assert!(find(&list, ":lolcat: hi").is_none());
        let names: Vec<&str> = suggest(&list, "lol").iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["lol", "lolcat", "catlol"]);
    }
}
