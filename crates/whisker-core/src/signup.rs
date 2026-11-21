//! Sign-up steps a server can ask for, beyond the ones that need nothing
//! from the person: the server's own captcha, shown in the app, and the
//! standard Matrix fallback (a step done on the server's own web page).

use base64::Engine;
use matrix_sdk::ruma::api::client::uiaa::AuthType;

/// The in-app captcha step (anoncat). Its picture comes with the server's
/// answer, as a `data:` URL in `params`, so nothing else is fetched (it
/// works over an onion address, and through the account's proxy):
/// `"params": {"org.anoncat.captcha": {"image": "data:image/png;base64,…"}}`.
/// The answer goes back as `{"type": CAPTCHA, "session": …, "response": …}`.
pub const CAPTCHA: &str = "org.anoncat.captcha";

/// Largest captcha picture taken (decoded).
const MAX_PICTURE: usize = 512 * 1024;

/// What a sign-up step is, in words.
pub fn describe(t: &AuthType) -> String {
    match t {
        AuthType::EmailIdentity => "an email address".to_owned(),
        AuthType::Msisdn => "a phone number".to_owned(),
        AuthType::ReCaptcha => "a captcha".to_owned(),
        AuthType::RegistrationToken => "a registration token".to_owned(),
        AuthType::Terms => "accepting its terms".to_owned(),
        t if t.as_str() == CAPTCHA => "its captcha".to_owned(),
        other => other.to_string(),
    }
}

/// The captcha picture in a sign-up answer's `params` (its JSON): PNG or
/// JPEG bytes from a `data:` URL, never a link to fetch.
pub fn captcha_image(params: Option<&str>) -> Option<Vec<u8>> {
    let v: serde_json::Value = serde_json::from_str(params?).ok()?;
    let url = v.get(CAPTCHA)?.get("image")?.as_str()?;
    let rest = url.strip_prefix("data:")?;
    let (head, data) = rest.split_once(',')?;
    let mime = head.strip_suffix(";base64")?;
    if !matches!(mime.to_ascii_lowercase().as_str(), "image/png" | "image/jpeg") || data.len() > MAX_PICTURE * 4 / 3 + 4 {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim()).ok()?;
    let png = bytes.starts_with(b"\x89PNG\r\n\x1a\n");
    let jpeg = bytes.starts_with(&[0xFF, 0xD8, 0xFF]);
    ((png || jpeg) && bytes.len() <= MAX_PICTURE).then_some(bytes)
}

/// The documents a terms step (`m.login.terms`) asks to accept: each one's
/// name and link (English if there is one), in the server's order.
pub fn terms(params: Option<&str>) -> Vec<(String, String)> {
    let Some(v) = params.and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok()) else { return Vec::new() };
    let Some(policies) = v.get("m.login.terms").and_then(|t| t.get("policies")).and_then(|p| p.as_object()) else { return Vec::new() };
    policies
        .iter()
        .filter_map(|(id, p)| {
            let p = p.as_object()?;
            let doc = p.get("en").or_else(|| p.values().find(|d| d.is_object()))?;
            let url = doc.get("url")?.as_str()?;
            url::Url::parse(url).ok().filter(|u| matches!(u.scheme(), "https" | "http"))?;
            Some((doc.get("name").and_then(|n| n.as_str()).unwrap_or(id).to_owned(), url.to_owned()))
        })
        .collect()
}

/// The server's own page for a step (`/_matrix/client/v3/auth/<type>/fallback/web`).
pub fn fallback_url(homeserver: &str, stage: &str, session: &str) -> String {
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    format!("{}/_matrix/client/v3/auth/{}/fallback/web?session={}", homeserver.trim_end_matches('/'), enc(stage), enc(session))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_only_inline_pictures() {
        let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nrest");
        let p = format!(r#"{{"org.anoncat.captcha": {{"image": "data:image/png;base64,{png}", "expires_in": 300}}}}"#);
        assert_eq!(captcha_image(Some(&p)).unwrap(), b"\x89PNG\r\n\x1a\nrest");
        // A link to fetch, another kind of file, or not what it says: no.
        assert!(captcha_image(Some(r#"{"org.anoncat.captcha": {"image": "https://evil.example/c.png"}}"#)).is_none());
        let svg = base64::engine::general_purpose::STANDARD.encode(b"<svg/>");
        assert!(captcha_image(Some(&format!(r#"{{"org.anoncat.captcha": {{"image": "data:image/svg+xml;base64,{svg}"}}}}"#))).is_none());
        let fake = base64::engine::general_purpose::STANDARD.encode(b"<html>");
        assert!(captcha_image(Some(&format!(r#"{{"org.anoncat.captcha": {{"image": "data:image/png;base64,{fake}"}}}}"#))).is_none());
        assert!(captcha_image(None).is_none());
    }

    #[test]
    fn reads_terms() {
        let p = r#"{"m.login.terms": {"policies": {"rules": {"version": "1", "en": {"name": "Server rules", "url": "https://cat.example/rules"}},
            "bad": {"version": "1", "en": {"name": "x", "url": "javascript:alert(1)"}}}}}"#;
        assert_eq!(terms(Some(p)), [("Server rules".to_owned(), "https://cat.example/rules".to_owned())]);
        assert!(terms(Some("{}")).is_empty());
    }

    #[test]
    fn fallback_page() {
        assert_eq!(
            fallback_url("https://hs.example/", "m.login.terms", "a b&c"),
            "https://hs.example/_matrix/client/v3/auth/m.login.terms/fallback/web?session=a%20b%26c"
        );
        assert_eq!(describe(&AuthType::from(CAPTCHA)), "its captcha");
    }
}
