//! Fixing typos as you type, like a phone keyboard: when a word is finished
//! (space or punctuation after it), a misspelling is swapped for the right
//! word. First a list of common ones (missing apostrophes, a lone "i"),
//! then any word that isn't in the English word list but is one slip away
//! from exactly one word that is: two letters swapped, one letter extra,
//! missing or wrong. When two words are equally close it's left alone.
//! Pressing Backspace right after a fix puts your word back (see
//! `fix_typing`).
//!
//! The word list is made from the SCOWL en_US Hunspell dictionary (see
//! `assets/words-LICENSE.txt`).

use std::collections::HashSet;
use std::sync::OnceLock;

static WORD_LIST: &str = include_str!("../assets/words.txt");

fn words() -> &'static HashSet<&'static str> {
    static WORDS: OnceLock<HashSet<&'static str>> = OnceLock::new();
    WORDS.get_or_init(|| WORD_LIST.lines().collect())
}

/// Loads the word list ahead of the first finished word.
pub fn warm() {
    std::thread::spawn(|| {
        words();
    });
}

/// The word `word` should become, keeping a capital first letter.
pub fn fix(word: &str) -> Option<String> {
    if word.is_empty() || word.len() > 20 || !word.chars().all(|c| c.is_alphabetic()) {
        return None;
    }
    if word == "i" {
        return Some("I".into());
    }
    let lower = word.to_lowercase();
    // Mixed case ("iPhone", "McDonald") is on purpose.
    let first_upper = word.chars().next().is_some_and(char::is_uppercase);
    let rest_lower = word.chars().skip(1).all(char::is_lowercase);
    if !rest_lower {
        return None;
    }
    let right = match FIXES.binary_search_by(|(w, _)| w.cmp(&lower.as_str())) {
        Ok(i) => FIXES[i].1.to_owned(),
        Err(_) => nearest(&lower)?,
    };
    if first_upper {
        let mut c = right.chars();
        Some(c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default())
    } else {
        Some(right)
    }
}

/// The one word `typed` is a slip away from, if it isn't a word itself.
/// Two words at the same distance is a guess, so none (unless one is a
/// swap, or a neighbouring key, and the other candidates are few).
fn nearest(typed: &str) -> Option<String> {
    let words = words();
    // Short words are too often names, slang or other languages.
    if typed.chars().count() < 4 || !typed.is_ascii() || words.contains(typed) {
        return None;
    }
    let b = typed.as_bytes();
    let n = b.len();
    let letters = b'a'..=b'z';
    let swapped = (0..n - 1).map(|i| {
        let mut v = b.to_vec();
        v.swap(i, i + 1);
        v
    });
    let extra = (0..n).map(|i| [&b[..i], &b[i + 1..]].concat());
    let missing = (0..=n).flat_map(|i| letters.clone().map(move |c| [&b[..i], &[c], &b[i..]].concat()));
    let next_key = (0..n).flat_map(|i| neighbours(b[i]).iter().map(move |&c| [&b[..i], &[c], &b[i + 1..]].concat()));
    let wrong = (0..n).flat_map(|i| letters.clone().map(move |c| [&b[..i], &[c], &b[i + 1..]].concat()));
    let found = |slips: Box<dyn Iterator<Item = Vec<u8>> + '_>| -> HashSet<String> {
        slips.filter_map(|v| String::from_utf8(v).ok()).filter(|w| w != typed && words.contains(w.as_str())).collect()
    };
    // Two letters swapped is the likeliest slip of all.
    let swaps = found(Box::new(swapped));
    if swaps.len() == 1 {
        return swaps.into_iter().next();
    }
    let mut near = found(Box::new(extra.chain(missing).chain(wrong)));
    near.extend(swaps);
    if near.len() == 1 {
        return near.into_iter().next();
    }
    // Several: only a wrong letter on a key next to the right one, if
    // that's a single word.
    let keyed = found(Box::new(next_key));
    if keyed.len() == 1 && near.len() <= 3 {
        return keyed.into_iter().next();
    }
    None
}

/// Keys next to `c` on a QWERTY keyboard.
fn neighbours(c: u8) -> &'static [u8] {
    match c {
        b'q' => b"wa",
        b'w' => b"qeas",
        b'e' => b"wrsd",
        b'r' => b"etdf",
        b't' => b"ryfg",
        b'y' => b"tugh",
        b'u' => b"yihj",
        b'i' => b"uojk",
        b'o' => b"ipkl",
        b'p' => b"ol",
        b'a' => b"qwsz",
        b's' => b"weadzx",
        b'd' => b"ersfxc",
        b'f' => b"rtdgcv",
        b'g' => b"tyfhvb",
        b'h' => b"yugjbn",
        b'j' => b"uihknm",
        b'k' => b"iojlm",
        b'l' => b"opk",
        b'z' => b"asx",
        b'x' => b"zsdc",
        b'c' => b"xdfv",
        b'v' => b"cfgb",
        b'b' => b"vghn",
        b'n' => b"bhjm",
        b'm' => b"njk",
        _ => b"",
    }
}

/// Characters that end a word (and trigger a fix).
pub fn ends_word(c: char) -> bool {
    c.is_whitespace() || matches!(c, '.' | ',' | '!' | '?' | ';' | ':' | ')')
}

/// Misspelling → word, sorted by misspelling (checked by a test).
const FIXES: &[(&str, &str)] = &[
    ("abotu", "about"),
    ("abscence", "absence"),
    ("accidently", "accidentally"),
    ("accomodate", "accommodate"),
    ("acheive", "achieve"),
    ("acknowlege", "acknowledge"),
    ("acording", "according"),
    ("acros", "across"),
    ("actualy", "actually"),
    ("adn", "and"),
    ("adress", "address"),
    ("agian", "again"),
    ("agred", "agreed"),
    ("alot", "a lot"),
    ("alraedy", "already"),
    ("alredy", "already"),
    ("alwasy", "always"),
    ("alwyas", "always"),
    ("amke", "make"),
    ("anohter", "another"),
    ("anwser", "answer"),
    ("apparantly", "apparently"),
    ("appearence", "appearance"),
    ("arent", "aren't"),
    ("arguement", "argument"),
    ("atleast", "at least"),
    ("awsome", "awesome"),
    ("basicly", "basically"),
    ("beacuse", "because"),
    ("becasue", "because"),
    ("becaus", "because"),
    ("becuase", "because"),
    ("becuse", "because"),
    ("befor", "before"),
    ("begining", "beginning"),
    ("beleive", "believe"),
    ("belive", "believe"),
    ("benifit", "benefit"),
    ("betwen", "between"),
    ("bizzare", "bizarre"),
    ("calender", "calendar"),
    ("cant", "can't"),
    ("carefull", "careful"),
    ("catagory", "category"),
    ("cemetary", "cemetery"),
    ("certianly", "certainly"),
    ("chnage", "change"),
    ("collegue", "colleague"),
    ("comming", "coming"),
    ("commited", "committed"),
    ("completly", "completely"),
    ("concious", "conscious"),
    ("couldnt", "couldn't"),
    ("cuase", "cause"),
    ("definately", "definitely"),
    ("definatly", "definitely"),
    ("definetly", "definitely"),
    ("definitly", "definitely"),
    ("didnt", "didn't"),
    ("diffrent", "different"),
    ("dissapoint", "disappoint"),
    ("doesnt", "doesn't"),
    ("doign", "doing"),
    ("dont", "don't"),
    ("embarass", "embarrass"),
    ("enviroment", "environment"),
    ("esle", "else"),
    ("everthing", "everything"),
    ("everytime", "every time"),
    ("excercise", "exercise"),
    ("existance", "existence"),
    ("experiance", "experience"),
    ("familar", "familiar"),
    ("finaly", "finally"),
    ("foriegn", "foreign"),
    ("freind", "friend"),
    ("freinds", "friends"),
    ("fwe", "few"),
    ("garantee", "guarantee"),
    ("gaurd", "guard"),
    ("goign", "going"),
    ("goverment", "government"),
    ("grammer", "grammar"),
    ("hadnt", "hadn't"),
    ("happend", "happened"),
    ("hasnt", "hasn't"),
    ("havent", "haven't"),
    ("hte", "the"),
    ("immediatly", "immediately"),
    ("independant", "independent"),
    ("interupt", "interrupt"),
    ("irrelevent", "irrelevant"),
    ("isnt", "isn't"),
    ("ive", "I've"),
    ("jsut", "just"),
    ("knowlege", "knowledge"),
    ("konw", "know"),
    ("libary", "library"),
    ("liek", "like"),
    ("lisence", "licence"),
    ("maintainance", "maintenance"),
    ("mispell", "misspell"),
    ("mroe", "more"),
    ("neccessary", "necessary"),
    ("necesary", "necessary"),
    ("noticable", "noticeable"),
    ("nto", "not"),
    ("occassion", "occasion"),
    ("occured", "occurred"),
    ("occurence", "occurrence"),
    ("ofthe", "of the"),
    ("ohter", "other"),
    ("peice", "piece"),
    ("persue", "pursue"),
    ("posession", "possession"),
    ("prefered", "preferred"),
    ("probaly", "probably"),
    ("probobly", "probably"),
    ("publically", "publicly"),
    ("realy", "really"),
    ("reccomend", "recommend"),
    ("recieve", "receive"),
    ("recieved", "received"),
    ("recomend", "recommend"),
    ("rember", "remember"),
    ("remeber", "remember"),
    ("resturant", "restaurant"),
    ("rythm", "rhythm"),
    ("saftey", "safety"),
    ("seperate", "separate"),
    ("shouldnt", "shouldn't"),
    ("similiar", "similar"),
    ("somethign", "something"),
    ("sucess", "success"),
    ("suprise", "surprise"),
    ("taht", "that"),
    ("teh", "the"),
    ("thatll", "that'll"),
    ("theres", "there's"),
    ("theyll", "they'll"),
    ("theyre", "they're"),
    ("theyve", "they've"),
    ("thier", "their"),
    ("thign", "thing"),
    ("thigns", "things"),
    ("thikn", "think"),
    ("thnak", "thank"),
    ("thnaks", "thanks"),
    ("tihs", "this"),
    ("tommorow", "tomorrow"),
    ("tommorrow", "tomorrow"),
    ("tomorow", "tomorrow"),
    ("tonite", "tonight"),
    ("truely", "truly"),
    ("tyhe", "the"),
    ("ubt", "but"),
    ("unfortunatly", "unfortunately"),
    ("unforunately", "unfortunately"),
    ("untill", "until"),
    ("usualy", "usually"),
    ("waht", "what"),
    ("wasnt", "wasn't"),
    ("werent", "weren't"),
    ("weve", "we've"),
    ("whats", "what's"),
    ("whcih", "which"),
    ("wich", "which"),
    ("wierd", "weird"),
    ("wihch", "which"),
    ("wiht", "with"),
    ("wouldnt", "wouldn't"),
    ("writting", "writing"),
    ("wrok", "work"),
    ("yeild", "yield"),
    ("youll", "you'll"),
    ("youre", "you're"),
    ("youve", "you've"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_is_sorted() {
        for w in FIXES.windows(2) {
            assert!(w[0].0 < w[1].0, "{} should come after {}", w[0].0, w[1].0);
        }
    }

    #[test]
    fn fixes() {
        assert_eq!(fix("teh").as_deref(), Some("the"));
        assert_eq!(fix("Teh").as_deref(), Some("The"));
        assert_eq!(fix("dont").as_deref(), Some("don't"));
        assert_eq!(fix("i").as_deref(), Some("I"));
        assert_eq!(fix("ive").as_deref(), Some("I've"));
        assert_eq!(fix("TEH"), None);
        assert_eq!(fix("the"), None);
        assert_eq!(fix("http"), None);
        assert_eq!(fix("don't"), None);
    }

    #[test]
    fn fixes_slips_from_the_word_list() {
        assert_eq!(fix("thnig").as_deref(), Some("thing"));
        assert_eq!(fix("turnming").as_deref(), Some("turning"));
        // "hing" and "hiding" are both one slip away: a guess, so left alone.
        assert_eq!(fix("hidng"), None);
        assert_eq!(fix("exspected").as_deref(), Some("expected"));
        assert_eq!(fix("messgae").as_deref(), Some("message"));
        assert_eq!(fix("intresting").as_deref(), Some("interesting"));
        // "correct" or "current"? Without knowing which is commoner, neither.
        assert_eq!(fix("currect"), None);
        assert_eq!(fix("Contexr").as_deref(), Some("Context"));
        // Real words, short words and close calls stay as typed.
        assert_eq!(fix("context"), None);
        assert_eq!(fix("ther"), None);
        assert_eq!(fix("meny"), None);
    }
}
