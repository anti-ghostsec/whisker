//! Fixing common typos as you type, like a phone keyboard: when a word is
//! finished (space or punctuation after it), a known misspelling is swapped
//! for the right word. Only sure fixes: misspellings that aren't words
//! themselves, missing apostrophes, and a lone "i". Pressing Backspace right
//! after a fix puts your word back (see `draft_edited`).

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
    let right = FIXES.binary_search_by(|(w, _)| w.cmp(&lower.as_str())).ok().map(|i| FIXES[i].1)?;
    if first_upper {
        let mut c = right.chars();
        Some(c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default())
    } else {
        Some(right.to_owned())
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
}
