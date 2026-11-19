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

/// Web links in a message, for "Open link" in the right-click menu.
pub fn links(body: &str) -> Vec<String> {
    let mut found = Vec::new();
    for word in body.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == '<' || c == '>') {
        if (word.starts_with("https://") || word.starts_with("http://")) && !found.iter().any(|f| f == word) {
            found.push(word.trim_end_matches(['.', ',', '!', '?', ';', ':', '"', '\'', ']']).to_owned());
        }
    }
    found.truncate(5);
    found
}

#[cfg(test)]
mod merged_tests {
    use super::*;

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
        assert_eq!(links("go to https://a.org/x. or (https://b.org)"), vec!["https://a.org/x", "https://b.org"]);
    }
}
