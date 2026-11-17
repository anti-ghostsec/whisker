//! Previews of web links in messages: a title, the site, a line of
//! description and a picture, like other chat apps show. YouTube videos get
//! their title and thumbnail (through YouTube's oEmbed answer, so there's no
//! need to load the whole watch page); other pages are read for their Open
//! Graph tags (`og:title`, `og:image`…).
//!
//! Everything is fetched by this device (never asked of the homeserver,
//! which would learn the links in encrypted rooms) through [`crate::safe_web`],
//! and through the account's proxy when it has one. The sites still see a
//! request from you (or your proxy).

use crate::safe_web::{self, Ask};
use anyhow::{Result, bail};
use reqwest::Url;
use std::time::Duration;

const PAGE_BYTES: usize = 768 * 1024;
const IMAGE_BYTES: usize = 6 * 1024 * 1024;
const USER_AGENT: &str = "Mozilla/5.0 (compatible; Whisker link preview)";
const TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkPreview {
    pub url: String,
    /// "youtube.com", or the page's own site name.
    pub site: String,
    pub title: String,
    pub description: String,
    /// The picture's bytes, not yet decoded (or checked: decode with limits).
    pub image: Option<Vec<u8>>,
    /// A video (shows a play mark on the picture).
    pub video: bool,
}

/// The YouTube video a link is for, if it's one.
pub fn youtube_id(url: &Url) -> Option<String> {
    let host = url.host_str()?.trim_start_matches("www.").trim_start_matches("m.").trim_start_matches("music.");
    let id = match host {
        "youtu.be" => url.path_segments()?.next()?.to_owned(),
        "youtube.com" | "youtube-nocookie.com" => {
            let mut segs = url.path_segments()?;
            match segs.next()? {
                "watch" => url.query_pairs().find(|(k, _)| k == "v")?.1.into_owned(),
                "shorts" | "embed" | "live" | "v" => segs.next()?.to_owned(),
                _ => return None,
            }
        }
        _ => return None,
    };
    (id.len() >= 6 && id.len() <= 20 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')).then_some(id)
}

/// Fetches a preview of `url`.
pub async fn fetch(url: &str, proxy: Option<&str>) -> Result<LinkPreview> {
    let parsed = Url::parse(url)?;
    if let Some(id) = youtube_id(&parsed) {
        return youtube(url, &id, proxy).await;
    }
    let page = safe_web::get(parsed, proxy, &ask("text/html,application/xhtml+xml;q=0.9,*/*;q=0.1", PAGE_BYTES)).await?;
    if !page.content_type.is_empty() && !page.content_type.contains("html") {
        // A link straight to a picture: show it.
        if page.content_type.starts_with("image/") && !page.truncated {
            let site = host_of(&page.url);
            return Ok(LinkPreview { url: url.into(), title: file_name(&page.url), site, image: Some(page.body), ..Default::default() });
        }
        bail!("not a web page ({})", page.content_type);
    }
    let html = String::from_utf8_lossy(&page.body);
    let tags = meta_tags(&html);
    let get = |names: &[&str]| names.iter().find_map(|n| tags.iter().find(|(k, v)| k == n && !v.trim().is_empty()).map(|(_, v)| v.trim().to_owned()));
    let title = get(&["og:title", "twitter:title"]).or_else(|| title_tag(&html)).unwrap_or_default();
    let description = get(&["og:description", "twitter:description", "description"]).unwrap_or_default();
    let site = get(&["og:site_name"]).unwrap_or_else(|| host_of(&page.url));
    let video = get(&["og:type"]).is_some_and(|t| t.starts_with("video"));
    if title.is_empty() && description.is_empty() {
        bail!("nothing to show");
    }
    let image = match get(&["og:image:secure_url", "og:image", "og:image:url", "twitter:image", "twitter:image:src"]).and_then(|i| page.url.join(&i).ok()) {
        Some(i) => picture(i, proxy).await,
        None => None,
    };
    Ok(LinkPreview { url: url.into(), site, title: clip(&title, 200), description: clip(&description, 300), image, video })
}

async fn youtube(url: &str, id: &str, proxy: Option<&str>) -> Result<LinkPreview> {
    let mut oembed = Url::parse("https://www.youtube.com/oembed?format=json")?;
    oembed.query_pairs_mut().append_pair("url", &format!("https://www.youtube.com/watch?v={id}"));
    let (title, author) = match safe_web::get(oembed, proxy, &ask("application/json", 64 * 1024)).await {
        Ok(got) => {
            let v: serde_json::Value = serde_json::from_slice(&got.body).unwrap_or_default();
            (v["title"].as_str().unwrap_or_default().to_owned(), v["author_name"].as_str().unwrap_or_default().to_owned())
        }
        // Private, removed or age-restricted: still show the thumbnail if there is one.
        Err(e) => {
            tracing::debug!("no oEmbed answer for a YouTube link: {e:#}");
            Default::default()
        }
    };
    let image = picture(Url::parse(&format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"))?, proxy).await;
    if title.is_empty() && image.is_none() {
        bail!("YouTube had nothing for that video");
    }
    Ok(LinkPreview { url: url.into(), site: "YouTube".into(), title: clip(&title, 200), description: author, image, video: true })
}

async fn picture(url: Url, proxy: Option<&str>) -> Option<Vec<u8>> {
    match safe_web::get(url, proxy, &ask("image/avif,image/webp,image/png,image/jpeg,image/gif,image/*;q=0.8", IMAGE_BYTES)).await {
        Ok(got) if !got.truncated && (got.content_type.is_empty() || got.content_type.starts_with("image/")) => Some(got.body),
        Ok(_) => None,
        Err(e) => {
            tracing::debug!("couldn't fetch a link preview picture: {e:#}");
            None
        }
    }
}

fn ask(accept: &str, max_bytes: usize) -> Ask<'_> {
    Ask { accept, headers: &[], max_bytes, user_agent: USER_AGENT, timeout: TIMEOUT }
}

fn host_of(url: &Url) -> String {
    url.host_str().unwrap_or_default().trim_start_matches("www.").to_owned()
}

fn file_name(url: &Url) -> String {
    url.path_segments().and_then(|mut s| s.next_back()).filter(|s| !s.is_empty()).unwrap_or_default().to_owned()
}

fn clip(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s,
    }
}

/// `<meta property|name="…" content="…">` pairs in the page's head, names
/// lower-cased and contents unescaped.
fn meta_tags(html: &str) -> Vec<(String, String)> {
    let head = &html[..html.find("</head>").or_else(|| html.find("<body")).unwrap_or(html.len())];
    let mut out = Vec::new();
    let lower = head.to_ascii_lowercase();
    let mut at = 0;
    while let Some(i) = lower[at..].find("<meta").map(|i| at + i) {
        let end = lower[i..].find('>').map_or(lower.len(), |e| i + e);
        let tag = &head[i..end];
        let name = attr(tag, "property").or_else(|| attr(tag, "name"));
        if let (Some(name), Some(content)) = (name, attr(tag, "content")) {
            out.push((name.to_ascii_lowercase(), unescape(&content)));
        }
        at = end;
    }
    out
}

fn title_tag(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let start = start + lower[start..].find('>')? + 1;
    let end = start + lower[start..].find("</title")?;
    Some(unescape(html[start..end].trim())).filter(|t| !t.is_empty())
}

/// The value of attribute `name` in a tag's text.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name).map(|i| from + i) {
        from = i + name.len();
        // A whole attribute name, followed by `=`.
        if !lower[..i].ends_with(|c: char| c.is_whitespace()) {
            continue;
        }
        let rest = tag[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let rest = rest.trim_start();
        return Some(match rest.chars().next() {
            Some(q @ ('"' | '\'')) => rest[1..].split(q).next().unwrap_or_default().to_owned(),
            _ => rest.split(|c: char| c.is_whitespace() || c == '>' || c == '/').next().unwrap_or_default().to_owned(),
        });
    }
    None
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(semi) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ => ent
                .strip_prefix("#x")
                .or_else(|| ent.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_youtube_videos() {
        let id = |u: &str| youtube_id(&Url::parse(u).unwrap());
        assert_eq!(id("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=1"), Some("dQw4w9WgXcQ".into()));
        assert_eq!(id("https://youtu.be/dQw4w9WgXcQ?si=x"), Some("dQw4w9WgXcQ".into()));
        assert_eq!(id("https://m.youtube.com/shorts/abcDEF_123"), Some("abcDEF_123".into()));
        assert_eq!(id("https://youtube.com/@channel"), None);
        assert_eq!(id("https://example.com/watch?v=dQw4w9WgXcQ"), None);
        assert_eq!(id("https://youtu.be/../etc"), None);
    }

    #[test]
    fn reads_open_graph_tags() {
        let html = r#"<html><head><title>Plain &amp; title</title>
            <meta property="og:title" content="Big &quot;news&quot;">
            <META NAME='description' CONTENT='A thing &#8212; here'>
            <meta content="https://x.org/a.png" property="og:image" />
            </head><body><meta property="og:title" content="not this"></body>"#;
        let tags = meta_tags(html);
        assert_eq!(tags[0], ("og:title".into(), "Big \"news\"".into()));
        assert_eq!(tags[1], ("description".into(), "A thing — here".into()));
        assert_eq!(tags[2], ("og:image".into(), "https://x.org/a.png".into()));
        assert_eq!(tags.len(), 3);
        assert_eq!(title_tag(html), Some("Plain & title".into()));
    }
}

#[cfg(test)]
mod live {
    /// Reaches YouTube and GitHub: run by hand with `--ignored`.
    #[tokio::test]
    #[ignore]
    async fn fetches_real_previews() {
        let yt = super::fetch("https://youtu.be/dQw4w9WgXcQ", None).await.unwrap();
        assert!(!yt.title.is_empty() && yt.image.is_some() && yt.video, "{yt:?}");
        let gh = super::fetch("https://github.com/rust-lang/rust", None).await.unwrap();
        assert!(!gh.title.is_empty() && gh.image.is_some(), "{:?}", (gh.title, gh.site));
    }
}
