//! Turns a message body (Markdown-ish, as Matrix clients send it) into
//! blocks the UI draws as plain, selectable text: paragraphs, quotes, code
//! blocks and headings, with the inline Markdown markers taken out.

/// Matches `kind` in the Slint `Block` struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text = 0,
    Quote = 1,
    Code = 2,
    Heading = 3,
    Blank = 4,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub kind: Kind,
    /// Markdown source for Text/Quote/Heading lines, raw text for Code.
    pub source: String,
}

/// Splits a body into lines and blocks (no Slint types, so it's testable).
pub fn split(body: &str) -> Vec<Line> {
    let mut out = Vec::new();
    let mut code: Option<Vec<&str>> = None;
    for raw in body.lines() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            match code.take() {
                Some(lines) => out.push(Line { kind: Kind::Code, source: lines.join("\n") }),
                None => code = Some(Vec::new()),
            }
            continue;
        }
        if let Some(lines) = code.as_mut() {
            lines.push(raw);
            continue;
        }
        let line = if raw.trim().is_empty() {
            Line { kind: Kind::Blank, source: String::new() }
        } else if let Some(q) = trimmed.strip_prefix('>') {
            Line { kind: Kind::Quote, source: q.strip_prefix(' ').unwrap_or(q).to_owned() }
        } else if let Some(h) = trimmed.strip_prefix("# ").or_else(|| trimmed.strip_prefix("## ")).or_else(|| trimmed.strip_prefix("### ")) {
            Line { kind: Kind::Heading, source: h.to_owned() }
        } else {
            Line { kind: Kind::Text, source: raw.to_owned() }
        };
        out.push(line);
    }
    if let Some(lines) = code {
        // Unclosed fence: still show it as code.
        out.push(Line { kind: Kind::Code, source: lines.join("\n") });
    }
    // Collapse runs of blank lines and trim them at the ends.
    out.dedup_by(|a, b| a.kind == Kind::Blank && b.kind == Kind::Blank);
    while out.first().is_some_and(|l| l.kind == Kind::Blank) {
        out.remove(0);
    }
    while out.last().is_some_and(|l| l.kind == Kind::Blank) {
        out.pop();
    }
    out
}

/// Lines merged into selectable blocks: consecutive text lines become one
/// block (so a drag can cover a whole paragraph or list), likewise quotes.
/// Inline Markdown markers are removed so the text reads cleanly.
pub fn merged(body: &str) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    for l in split(body) {
        let source = match l.kind {
            Kind::Code => l.source,
            _ => plain_inline(&l.source),
        };
        match out.last_mut() {
            Some(prev) if prev.kind == l.kind && matches!(l.kind, Kind::Text | Kind::Quote) => {
                prev.source.push('\n');
                prev.source.push_str(&source);
            }
            _ => out.push(Line { kind: l.kind, source }),
        }
    }
    out
}

/// Markdown inline syntax to plain text: `[t](url)` → `t (url)`, list
/// markers → bullets, and `**`, `__`, `~~` and backticks dropped.
pub fn plain_inline(s: &str) -> String {
    let mut line = s.to_owned();
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")).or_else(|| trimmed.strip_prefix("+ ")) {
        line = format!("{}• {rest}", &line[..indent]);
    }
    // Links: [text](url)
    let mut out = String::with_capacity(line.len());
    let mut rest = line.as_str();
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find("](").map(|i| open + i) else { break };
        let Some(end) = rest[close..].find(')').map(|i| close + i) else { break };
        let (text, url) = (&rest[open + 1..close], &rest[close + 2..end]);
        out.push_str(&rest[..open]);
        if text == url || text.is_empty() {
            out.push_str(url);
        } else {
            out.push_str(&format!("{text} ({url})"));
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    for m in ["**", "__", "~~", "`"] {
        out = out.replace(m, "");
    }
    out
}

/// Where the web addresses in `text` are: byte ranges, in order.
fn link_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = text[i..].find("http") {
        let start = i + off;
        let tail = &text[start..];
        let boundary = text[..start].chars().next_back().is_none_or(|c| c.is_whitespace() || "(<\"'[".contains(c));
        if boundary && (tail.starts_with("https://") || tail.starts_with("http://")) {
            let len = tail.find(|c: char| c.is_whitespace() || c == '<' || c == '>' || c == '"').unwrap_or(tail.len());
            let mut url = &tail[..len];
            // Sentence punctuation after it isn't part of it; nor is a
            // closing bracket it didn't open.
            loop {
                let trimmed = url.trim_end_matches(['.', ',', '!', '?', ';', ':', '\'', ']']);
                let trimmed = if trimmed.ends_with(')') && !trimmed.contains('(') { &trimmed[..trimmed.len() - 1] } else { trimmed };
                if trimmed.len() == url.len() {
                    break;
                }
                url = trimmed;
            }
            if url.len() > "https://".len() {
                out.push(start..start + url.len());
                i = start + url.len();
                continue;
            }
        }
        i = start + 4;
    }
    out
}

/// `text` as Markdown for a Slint `StyledText`, with its web addresses as
/// links and everything else shown exactly as written (escaped). `None`
/// when it has no links (it's then shown as plain, selectable text).
pub fn linked_markdown(text: &str) -> Option<String> {
    let ranges = link_ranges(text);
    if ranges.is_empty() {
        return None;
    }
    fn plain(out: &mut String, t: &str) {
        for (n, line) in t.split('\n').enumerate() {
            if n > 0 {
                // A hard line break: lines stay lines.
                out.push_str("\\\n");
            }
            // Leading spaces would make an indented code block.
            let line = if out.is_empty() || out.ends_with('\n') { line.trim_start() } else { line };
            for c in line.chars() {
                if c.is_ascii_punctuation() {
                    out.push('\\');
                }
                out.push(c);
            }
        }
    }
    let mut out = String::new();
    let mut at = 0;
    let mut any = false;
    for r in ranges {
        // Shown exactly as it will open: the parsed address (look-alike
        // letters in the name as punycode, `xn--…`). The link target is
        // escaped too, so Markdown can't turn `&#64;` or the like into
        // something else when it's clicked.
        let Some(url) = clean_link(&text[r.clone()]) else { continue };
        plain(&mut out, &text[at..r.start]);
        out.push('[');
        plain(&mut out, &url);
        out.push_str("](<");
        for c in url.chars() {
            if c.is_ascii_punctuation() {
                out.push('\\');
            }
            out.push(c);
        }
        out.push_str(">)");
        at = r.end;
        any = true;
    }
    plain(&mut out, &text[at..]);
    any.then_some(out)
}

/// A web address as it really is: parsed and written back out (the host
/// in punycode when it has non-ASCII letters, which can look like others).
/// `None` for one that isn't a plain http(s) address.
pub fn clean_link(raw: &str) -> Option<String> {
    let u = url::Url::parse(raw).ok()?;
    matches!(u.scheme(), "http" | "https").then(|| u.as_str().to_owned()).filter(|_| u.host_str().is_some())
}

/// Web links in a message, for "Open link" in the right-click menu.
pub fn links(body: &str) -> Vec<String> {
    let mut found = Vec::new();
    for word in body.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == '<' || c == '>') {
        if (word.starts_with("https://") || word.starts_with("http://")) && !found.iter().any(|f| f == word) {
            // As it will really open (punycode for look-alike letters).
            let raw = word.trim_end_matches(['.', ',', '!', '?', ';', ':', '"', '\'', ']']);
            if let Some(u) = clean_link(raw).filter(|u| !found.contains(u)) {
                found.push(u);
            }
        }
    }
    found.truncate(5);
    found
}

#[cfg(test)]
mod merged_tests {
    use super::*;

    #[test]
    fn links_become_markdown_links() {
        assert_eq!(linked_markdown("no links here"), None);
        assert_eq!(linked_markdown("see https://a.org/x."), Some("see [https\\:\\/\\/a\\.org\\/x](<https\\:\\/\\/a\\.org\\/x>)\\.".to_owned()));
        // Everything else is shown as written: no Markdown, no HTML.
        let md = linked_markdown("*hi* <b> (https://b.org)").unwrap();
        assert!(md.starts_with("\\*hi\\* \\<b\\> \\("), "{md}");
        assert!(md.ends_with("](<https\\:\\/\\/b\\.org\\/>)\\)"), "{md}");
        assert_eq!(link_ranges("x https://w.org/a_(b) y").len(), 1);
        assert_eq!(&"x https://w.org/a_(b) y"[link_ranges("x https://w.org/a_(b) y")[0].clone()], "https://w.org/a_(b)");
        assert!(link_ranges("nohttps://x.org").is_empty());
        // What's shown is what opens: no entity tricks, and look-alike
        // letters in punycode.
        let md = linked_markdown("https://bank.com&#64;evil.example").unwrap();
        assert!(!md.contains("&#64;evil"), "{md}");
        // Shown and opened the same: the parsed address.
        assert!(md.contains(r"[https\:\/\/bank\.com\&\/\#64\;evil\.example](<https\:\/\/bank\.com\&\/\#64\;evil\.example>)"), "{md}");
        let md = linked_markdown("https://аpple.com/").unwrap();
        assert!(md.contains("xn\\-\\-"), "{md}");
        assert!(!md.contains('а'), "{md}");
    }

    #[test]
    fn splits_blocks() {
        let l = split("hi **there**\n\n\n> quoted\n> more\n```\nlet x = 1;\n\n```\n# Title\n- item");
        let kinds: Vec<_> = l.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            [Kind::Text, Kind::Blank, Kind::Quote, Kind::Quote, Kind::Code, Kind::Heading, Kind::Text]
        );
        assert_eq!(l[2].source, "quoted");
        assert_eq!(l[4].source, "let x = 1;\n");
        assert_eq!(l[5].source, "Title");
    }

    #[test]
    fn merges_and_strips() {
        let m = merged("hi **bold** and `code`\n- one\n- two\n> q1\n> q2\n```\nx\n```");
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].source, "hi bold and code\n• one\n• two");
        assert_eq!(m[1].source, "q1\nq2");
        assert_eq!(m[2].kind, Kind::Code);
        assert_eq!(plain_inline("see [docs](https://x.org) now"), "see docs (https://x.org) now");
        assert_eq!(links("go to https://a.org/x. or (https://b.org)"), vec!["https://a.org/x", "https://b.org/"]);
    }
}
