//! Spell checking the message box: words not in the English word list get
//! a red line under them once you stop typing, and right-clicking one
//! offers the nearest words (see `underline` in `select.rs`).
//!
//! Uses the autocorrect's word list (SCOWL en_US). Words you add are kept
//! in the vault, so they're encrypted like the rest of your data.
//!
//! Left alone: links, @people, #rooms, :emoji:, `code`, numbers, words in
//! capitals or mixed case (names, acronyms), single letters, and the word
//! the cursor is still in.

use crate::autocorrect;
use std::collections::HashSet;
use std::sync::Mutex;

/// Where words you added are kept in the vault.
pub const VAULT_KEY: &str = "spell-words";

/// Words you added, from every unlocked vault (lowercase).
static CUSTOM: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Adds words from a vault that was just unlocked.
pub fn add_custom(words: impl IntoIterator<Item = String>) {
    CUSTOM
        .lock()
        .unwrap()
        .get_or_insert_with(HashSet::new)
        .extend(words.into_iter().map(|w| w.to_lowercase()));
}

/// Forgets the added words (on lock).
pub fn clear_custom() {
    *CUSTOM.lock().unwrap() = None;
}

/// Most suggestions offered for a word.
const MAX_SUGGESTIONS: usize = 6;

/// Whether `word` (letters, maybe with an apostrophe) is spelled right.
pub fn known(word: &str) -> bool {
    let lower = word.to_lowercase();
    let words = autocorrect::words();
    if words.contains(lower.as_str())
        || CUSTOM
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|c| c.contains(&lower))
    {
        return true;
    }
    // The list has no contractions or possessives: "can't" is "can",
    // "isn't" is "is", "cat's" is "cat".
    let lower = lower.replace('’', "'");
    if let Some((base, end)) = lower.split_once('\'') {
        return match end {
            "t" => {
                words.contains(base)
                    || base.strip_suffix('n').is_some_and(|b| words.contains(b))
                    || autocorrect::is_fix_target(&lower)
            }
            "s" | "re" | "ve" | "ll" | "d" | "m" | "" => {
                words.contains(base) || autocorrect::is_fix_target(&lower)
            }
            _ => autocorrect::is_fix_target(&lower),
        };
    }
    false
}

/// Byte ranges of the misspelled words in `text`. `cursor` is the cursor's
/// byte offset: the word it's in (or right after) is still being typed.
pub fn misspelled(text: &str, cursor: Option<usize>) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (start, end) in words_in(text) {
        if cursor.is_some_and(|c| start <= c && c <= end) {
            continue;
        }
        let word = &text[start..end];
        if checkable(word) && !known(word) {
            out.push((start, end));
        }
    }
    out
}

/// The misspelled word at byte `offset` in `text`, as its range.
pub fn misspelled_at(text: &str, offset: usize) -> Option<(usize, usize)> {
    words_in(text)
        .into_iter()
        .find(|&(s, e)| s <= offset && offset <= e)
        .filter(|&(s, e)| checkable(&text[s..e]) && !known(&text[s..e]))
}

/// Words worth checking: lowercase, or capitalized like a sentence's
/// first word. Mixed case and capitals are names and acronyms.
fn checkable(word: &str) -> bool {
    let letters: Vec<char> = word.chars().filter(|c| c.is_alphabetic()).collect();
    letters.len() > 1 && letters.iter().skip(1).all(|c| c.is_lowercase()) && word.len() <= 30
}

/// Byte ranges of the words in `text`, leaving out what isn't prose.
fn words_in(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut in_code = false;
    // Whitespace-separated pieces first, to skip links, mentions and such.
    let mut i = 0;
    for piece in text.split_inclusive(char::is_whitespace) {
        let at = i;
        i += piece.len();
        let piece = piece.trim_end();
        let ticks = piece.matches('`').count();
        let was_code = in_code;
        if ticks % 2 == 1 {
            in_code = !in_code;
        }
        if was_code || ticks > 0 || skip_piece(piece) {
            continue;
        }
        // Words inside the piece: letters with apostrophes between them.
        let mut start = None;
        let chars: Vec<(usize, char)> = piece.char_indices().collect();
        for (k, &(off, c)) in chars.iter().enumerate() {
            let apostrophe = matches!(c, '\'' | '’')
                && start.is_some()
                && chars.get(k + 1).is_some_and(|(_, n)| n.is_alphabetic());
            if c.is_alphabetic() || apostrophe {
                start.get_or_insert(off);
            } else {
                if let Some(s) = start.take() {
                    push_word(&mut out, piece, at, s, off);
                }
            }
        }
        if let Some(s) = start {
            push_word(&mut out, piece, at, s, piece.len());
        }
    }
    out
}

/// Adds a word unless it's glued to digits or underscores ("abc123", "a_b").
fn push_word(out: &mut Vec<(usize, usize)>, piece: &str, at: usize, s: usize, e: usize) {
    let before = piece[..s].chars().next_back();
    let after = piece[e..].chars().next();
    if [before, after]
        .iter()
        .flatten()
        .any(|c| c.is_ascii_digit() || *c == '_')
    {
        return;
    }
    out.push((at + s, at + e));
}

/// Pieces that aren't words: links, addresses, mentions, rooms, emoji
/// codes, file paths.
fn skip_piece(p: &str) -> bool {
    let p = p.trim_start_matches(['(', '[', '"', '\'', '*', '_']);
    p.contains("://")
        || p.starts_with("www.")
        || p.starts_with(['@', '#', '!', '/', '~', '$'])
        || (p.starts_with(':') && p.len() > 1)
        || p.contains('@')
        || p.contains('\\')
        || (p.contains('.')
            && p.trim_end_matches(['.', ',', '!', '?', ')', ']', '"', '\''])
                .contains('.'))
}

/// The nearest words to a misspelling, likeliest first, with the typed
/// word's capital kept.
pub fn suggestions(typed: &str) -> Vec<String> {
    let lower = typed.to_lowercase();
    let words = autocorrect::words();
    let mut out: Vec<String> = Vec::new();
    let add = |w: String, out: &mut Vec<String>| {
        if w != lower && !out.contains(&w) && words.contains(w.as_str()) {
            out.push(w);
        }
    };
    if let Some(fix) = autocorrect::listed_fix(&lower) {
        add(fix.to_owned(), &mut out);
        // Contractions aren't in the list, but are the right answer.
        if !out.iter().any(|w| w == fix) {
            out.push(fix.to_owned());
        }
    }
    let first = edits(&lower);
    for w in first.iter() {
        add(w.clone(), &mut out);
    }
    // Nothing one slip away: two slips, for longer words.
    if out.is_empty() && (5..=14).contains(&lower.chars().count()) {
        let mut seen = HashSet::new();
        for w in &first {
            for w2 in edits(w) {
                if seen.insert(w2.clone()) {
                    add(w2, &mut out);
                }
                if out.len() >= MAX_SUGGESTIONS * 3 {
                    break;
                }
            }
        }
        // Closest in length first.
        out.sort_by_key(|w| (w.len().abs_diff(lower.len()), w.clone()));
    }
    out.truncate(MAX_SUGGESTIONS);
    let capital = typed.chars().next().is_some_and(char::is_uppercase);
    out.into_iter()
        .map(|w| {
            if capital {
                let mut c = w.chars();
                c.next()
                    .map(|f| f.to_uppercase().chain(c).collect())
                    .unwrap_or_default()
            } else {
                w
            }
        })
        .collect()
}

/// Every string one slip from `w`, likeliest slips first: two letters
/// swapped, a neighbouring key, a letter missing, one extra, any other
/// wrong letter.
fn edits(w: &str) -> Vec<String> {
    let c: Vec<char> = w.chars().collect();
    let n = c.len();
    let letters = 'a'..='z';
    let mut out = Vec::new();
    let s = |v: Vec<char>| v.into_iter().collect::<String>();
    for i in 0..n.saturating_sub(1) {
        let mut v = c.clone();
        v.swap(i, i + 1);
        out.push(s(v));
    }
    for i in 0..n {
        let ch = c[i];
        if ch.is_ascii() {
            for &k in autocorrect::neighbours(ch as u8) {
                let mut v = c.clone();
                v[i] = k as char;
                out.push(s(v));
            }
        }
    }
    for i in 0..=n {
        for l in letters.clone() {
            let mut v = c.clone();
            v.insert(i, l);
            out.push(s(v));
        }
    }
    for i in 0..n {
        let mut v = c.clone();
        v.remove(i);
        out.push(s(v));
    }
    for i in 0..n {
        for l in letters.clone() {
            let mut v = c.clone();
            v[i] = l;
            out.push(s(v));
        }
    }
    out
}

/// A wavy line `width` pixels long, as SVG path commands for a Path
/// `height` 3 high.
pub fn squiggle(width: f32) -> String {
    let mut d = String::from("M 0 2");
    let mut x = 0.0;
    let mut up = true;
    while x < width {
        let nx = (x + 2.0f32).min(width);
        d.push_str(&format!(" L {nx:.1} {}", if up { 0.5 } else { 2.5 }));
        up = !up;
        x = nx;
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marked(text: &str) -> Vec<&str> {
        misspelled(text, None)
            .into_iter()
            .map(|(s, e)| &text[s..e])
            .collect()
    }

    #[test]
    fn finds_misspelled_words() {
        assert_eq!(marked("I recieve teh mail"), vec!["recieve", "teh"]);
        assert_eq!(marked("Hello there, how are you?"), Vec::<&str>::new());
    }

    #[test]
    fn leaves_contractions_and_names_alone() {
        assert_eq!(
            marked("I can't, isn't it, the cat's toy, we're, I'm, they'll"),
            Vec::<&str>::new()
        );
        assert_eq!(marked("NASA and McDonald met iPhone"), Vec::<&str>::new());
    }

    #[test]
    fn leaves_links_and_code_alone() {
        assert_eq!(
            marked(
                "see https://exmple.org/qwzx and @bob:x.org #room:x.org :smiel: `fn qwzx()` abc123 x_y file.rs"
            ),
            Vec::<&str>::new()
        );
        assert_eq!(marked("say `qwzx yyzz` then qwzx"), vec!["qwzx"]);
    }

    #[test]
    fn skips_the_word_being_typed() {
        let t = "teh qwzx";
        assert_eq!(misspelled(t, Some(t.len())).len(), 1);
        assert_eq!(misspelled(t, Some(2)).len(), 1);
        assert_eq!(misspelled(t, None).len(), 2);
    }

    #[test]
    fn finds_the_word_under_the_mouse() {
        let t = "a recieve b";
        assert_eq!(misspelled_at(t, 4), Some((2, 9)));
        assert_eq!(misspelled_at(t, 0), None);
    }

    #[test]
    fn suggests_the_nearest_words() {
        assert_eq!(
            suggestions("recieve").first().map(String::as_str),
            Some("receive")
        );
        assert!(suggestions("Teh").contains(&"The".to_owned()));
        assert!(suggestions("definately").iter().any(|w| w == "definitely"));
        assert!(suggestions("helo").contains(&"hello".to_owned()));
    }

    #[test]
    fn remembers_added_words() {
        assert!(!known("whiskerish"));
        add_custom(["Whiskerish".to_owned()]);
        assert!(known("whiskerish"));
        clear_custom();
        assert!(!known("whiskerish"));
    }

    #[test]
    fn draws_a_squiggle() {
        assert!(squiggle(10.0).starts_with("M 0 2 L 2.0 0.5 L 4.0 2.5"));
    }
}
