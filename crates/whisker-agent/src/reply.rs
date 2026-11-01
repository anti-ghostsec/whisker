//! Cleaning a model reply before it goes to a room.
//!
//! Character cards often ask the model to keep a hidden tracker, written as
//! a `<state>…</state>` block. That block is for the model, never the chat:
//! [`hide_state`] takes it out (closed or cut off, any spelling of the tag)
//! and hands it back so the next prompt can show it to the model again.

/// The reply without any `<state…>` blocks, and the last block's contents.
/// An opening tag with no closing one (a reply cut short) hides everything
/// after it; a stray closing tag is dropped.
pub fn hide_state(text: &str) -> (String, Option<String>) {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut state = None;
    let mut i = 0;
    while let Some(at) = find_tag(&lower, i, "<state") {
        out.push_str(&text[i..at]);
        let open_end = lower[at..].find('>').map_or(text.len(), |e| at + e + 1);
        match find_tag(&lower, open_end, "</state") {
            Some(close) => {
                state = Some(text[open_end..close].trim().to_owned());
                i = lower[close..].find('>').map_or(text.len(), |e| close + e + 1);
            }
            None => {
                state = Some(text[open_end.min(text.len())..].trim().to_owned());
                i = text.len();
            }
        }
    }
    out.push_str(&text[i..]);
    // A closing tag left over from a block opened in an earlier reply.
    let mut cleaned = String::with_capacity(out.len());
    let lower = out.to_ascii_lowercase();
    let mut j = 0;
    while let Some(at) = find_tag(&lower, j, "</state") {
        cleaned.push_str(&out[j..at]);
        j = lower[at..].find('>').map_or(out.len(), |e| at + e + 1);
    }
    cleaned.push_str(&out[j..]);
    (tidy(&cleaned), state.filter(|s| !s.is_empty()))
}

/// The next `tag` at or after `from` that is really that tag: `<state>`,
/// `<state ` or `<state_x>`, but not `<statement>`.
fn find_tag(lower: &str, mut from: usize, tag: &str) -> Option<usize> {
    while from <= lower.len() {
        let at = from + lower.get(from..)?.find(tag)?;
        let next = lower[at + tag.len()..].chars().next();
        if next.is_none_or(|c| !c.is_ascii_alphanumeric()) {
            return Some(at);
        }
        from = at + tag.len();
    }
    None
}

/// Drops code fences the state left empty and runs of blank lines.
fn tidy(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut keep = vec![true; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim_start().starts_with("```") {
            let close = (i + 1..lines.len()).find(|&j| lines[j].trim() == "```");
            if let Some(c) = close {
                if lines[i + 1..c].iter().all(|l| l.trim().is_empty()) {
                    keep[i..=c].iter_mut().for_each(|k| *k = false);
                }
                i = c + 1;
                continue;
            }
        }
        i += 1;
    }
    let mut out = String::new();
    let mut blank = 0;
    for (l, k) in lines.iter().zip(keep) {
        if !k {
            continue;
        }
        if l.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(l.trim_end());
        out.push('\n');
    }
    out.trim().to_owned()
}

/// `text` cut to at most `max` characters (0: no limit), ending at the last
/// sentence or line end that fits, so a long reply doesn't stop mid-word.
pub fn cap(text: &str, max: usize) -> String {
    if max == 0 || text.chars().count() <= max {
        return text.to_owned();
    }
    let end = text.char_indices().nth(max).map_or(text.len(), |(i, _)| i);
    let head = &text[..end];
    let cut = head
        .char_indices()
        .filter(|&(i, c)| matches!(c, '.' | '!' | '?' | '…' | '\n' | '*' | '"' | '”') && i > end / 3)
        .map(|(i, c)| i + c.len_utf8())
        .next_back();
    match cut {
        Some(c) => head[..c].trim_end().to_owned(),
        None => {
            let word = head.rfind(char::is_whitespace).filter(|&w| w > end / 3).unwrap_or(end);
            format!("{}…", head[..word].trim_end())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_never_reaches_the_chat() {
        let (shown, state) = hide_state("Hello there.\n\n<state>\nmood: happy\n</state>");
        assert_eq!((shown.as_str(), state.as_deref()), ("Hello there.", Some("mood: happy")));
        // Any case, attributes, cut off, and in a code fence.
        assert_eq!(hide_state("Hi <STATE turn=\"3\">x</State> there").0, "Hi  there");
        assert_eq!(hide_state("Hi.\n<state>\nmood: hap").0, "Hi.");
        assert_eq!(hide_state("Hi.\n<state").0, "Hi.");
        assert_eq!(hide_state("Hi.\n```xml\n<state>a</state>\n```\nBye.").0, "Hi.\nBye.");
        assert_eq!(hide_state("<state_tracker>a</state_tracker>Hi").0, "Hi");
        assert_eq!(hide_state("old</state> Hi").0, "old Hi");
        // Not the tag.
        assert_eq!(hide_state("a <statement> here").0, "a <statement> here");
        assert_eq!(hide_state("<state>only</state>").0, "");
    }

    #[test]
    fn caps_at_a_sentence() {
        assert_eq!(cap("short", 0), "short");
        assert_eq!(cap("One two. Three four five six.", 20), "One two.");
        assert_eq!(cap("aaaa bbbb cccc dddd", 12), "aaaa bbbb…");
        assert_eq!(cap("ééééé", 3), "ééé…");
    }
}
