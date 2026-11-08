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
