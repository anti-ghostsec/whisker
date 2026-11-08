//! The emoji offered by the picker, with words to search them by. A short,
//! hand-picked list (the everyday ones) rather than all of Unicode.

mod pictures {
    include!(concat!(env!("OUT_DIR"), "/emoji_pictures.rs"));
}

/// The bundled picture (72×72 PNG, Noto Emoji) for an emoji, if there is one.
pub fn picture(emoji: &str) -> Option<&'static [u8]> {
    let stem = format!("emoji_u{}", emoji.chars().filter(|c| *c != '\u{fe0f}').map(|c| format!("{:x}", c as u32)).collect::<Vec<_>>().join("_"));
    pictures::PICTURES.binary_search_by(|(n, _)| (*n).cmp(stem.as_str())).ok().map(|i| pictures::PICTURES[i].1)
}

/// The emoji in `text` when it's nothing but (at most `max`) emoji we have
/// pictures for, so the message can show them big.
pub fn only_emoji(text: &str, max: usize) -> Option<Vec<&'static str>> {
    let mut rest = text.trim();
    let mut out = Vec::new();
    while !rest.is_empty() {
        // The longest known emoji at the start (sequences before their parts).
        let e = ALL.iter().map(|(e, _)| *e).filter(|e| rest.starts_with(*e) || rest.starts_with(e.trim_end_matches('\u{fe0f}'))).max_by_key(|e| e.len())?;
        let len = if rest.starts_with(e) { e.len() } else { e.trim_end_matches('\u{fe0f}').len() };
        picture(e)?;
        out.push(e);
        rest = rest[len..].trim_start_matches('\u{fe0f}').trim_start();
        if out.len() > max {
            return None;
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Shown first when reacting from a message's menu.
pub const QUICK: [&str; 6] = ["👍", "❤️", "😂", "😮", "😢", "🎉"];

/// What each of [`QUICK`] says, next to its picture in menus.
pub const QUICK_LABELS: [&str; 6] = ["Like", "Love", "Funny", "Wow", "Sad", "Party"];

/// (emoji, search words), in picker order.
pub const ALL: &[(&str, &str)] = &[
    // Smileys
    ("😀", "grin smile happy"),
    ("😃", "smile happy joy"),
    ("😄", "smile happy laugh"),
    ("😁", "grin beam teeth"),
    ("😆", "laugh squint lol"),
    ("😅", "sweat smile nervous"),
    ("🤣", "rofl rolling laugh lol"),
    ("😂", "joy tears laugh lol cry"),
    ("🙂", "slight smile"),
    ("🙃", "upside down silly"),
    ("😉", "wink"),
    ("😊", "blush smile happy"),
    ("😇", "angel innocent halo"),
    ("🥰", "love hearts adore"),
    ("😍", "heart eyes love crush"),
    ("🤩", "star struck wow excited"),
    ("😘", "kiss blow love"),
    ("😋", "yum tasty tongue"),
    ("😛", "tongue out"),
    ("😜", "wink tongue crazy"),
    ("🤪", "zany crazy goofy"),
    ("😝", "squint tongue"),
    ("🤑", "money rich dollar"),
    ("🤗", "hug hugging"),
    ("🤭", "giggle oops hand over mouth"),
    ("🤫", "shush quiet secret"),
    ("🤔", "thinking hmm think"),
    ("🤐", "zipper mouth secret"),
    ("🤨", "raised eyebrow suspicious skeptic"),
    ("😐", "neutral meh"),
    ("😑", "expressionless blank"),
    ("😶", "no mouth silent speechless"),
    ("😏", "smirk smug"),
    ("😒", "unamused meh annoyed"),
    ("🙄", "eye roll whatever"),
    ("😬", "grimace awkward yikes"),
    ("🤥", "lying liar pinocchio"),
    ("😌", "relieved calm content"),
    ("😔", "pensive sad"),
    ("😪", "sleepy tired"),
    ("🤤", "drool"),
    ("😴", "sleep zzz tired"),
    ("😷", "mask sick ill"),
    ("🤒", "fever sick thermometer"),
    ("🤢", "nauseated sick gross"),
    ("🤮", "vomit sick puke gross"),
    ("🥵", "hot heat sweat"),
    ("🥶", "cold freezing"),
    ("🥴", "woozy drunk dizzy"),
    ("😵", "dizzy dead knocked out"),
    ("🤯", "mind blown exploding head"),
    ("🤠", "cowboy"),
    ("🥳", "party celebrate birthday"),
    ("😎", "cool sunglasses"),
    ("🤓", "nerd geek glasses"),
    ("🧐", "monocle curious inspect"),
    ("😕", "confused unsure"),
    ("😟", "worried concerned"),
    ("🙁", "frown sad"),
    ("😮", "wow surprised open mouth oh"),
    ("😯", "hushed surprised"),
    ("😲", "astonished shocked"),
    ("😳", "flushed embarrassed blush"),
    ("🥺", "pleading puppy eyes please"),
    ("😦", "frowning open mouth"),
    ("😧", "anguished"),
    ("😨", "fearful scared"),
    ("😰", "anxious sweat worried"),
    ("😥", "sad relieved disappointed"),
    ("😢", "cry sad tear"),
    ("😭", "sob crying loud sad"),
    ("😱", "scream fear shocked"),
    ("😖", "confounded"),
    ("😣", "persevere struggle"),
    ("😞", "disappointed sad"),
    ("😓", "downcast sweat"),
    ("😩", "weary tired"),
    ("😫", "tired exhausted"),
    ("🥱", "yawn bored tired"),
    ("😤", "triumph huff steam angry"),
    ("😡", "pout angry mad rage"),
    ("😠", "angry mad"),
    ("🤬", "swearing cursing angry"),
    ("😈", "devil smiling evil horns"),
    ("👿", "imp angry devil"),
    ("💀", "skull dead dying lol"),
    ("☠️", "skull crossbones danger"),
    ("💩", "poop poo"),
    ("🤡", "clown"),
    ("👹", "ogre monster"),
    ("👻", "ghost boo spooky halloween"),
    ("👽", "alien ufo"),
    ("🤖", "robot bot ai"),
    ("😺", "cat smile"),
    ("😸", "cat grin"),
    ("😹", "cat joy tears"),
    ("😻", "cat heart eyes love"),
    ("😼", "cat smirk"),
    ("🙀", "cat weary shocked"),
    ("😿", "cat crying sad"),
    ("😾", "cat pouting angry"),
    ("🙈", "see no evil monkey"),
    ("🙉", "hear no evil monkey"),
    ("🙊", "speak no evil monkey"),
    // Hearts and symbols
    ("❤️", "heart love red"),
    ("🧡", "orange heart"),
    ("💛", "yellow heart"),
    ("💚", "green heart"),
    ("💙", "blue heart"),
    ("💜", "purple heart"),
    ("🖤", "black heart"),
    ("🤍", "white heart"),
    ("🤎", "brown heart"),
    ("💔", "broken heart"),
    ("❣️", "heart exclamation"),
    ("💕", "two hearts love"),
    ("💞", "revolving hearts"),
    ("💓", "beating heart"),
    ("💗", "growing heart"),
    ("💖", "sparkling heart"),
    ("💘", "heart arrow cupid"),
    ("💝", "heart ribbon gift"),
    ("💯", "hundred 100 perfect"),
    ("💢", "anger"),
    ("💥", "collision boom"),
    ("💫", "dizzy star"),
    ("💦", "sweat droplets water"),
    ("💨", "dash wind fast"),
    ("💬", "speech bubble chat"),
    ("💭", "thought bubble"),
    ("💤", "zzz sleep"),
    ("✨", "sparkles shiny magic"),
    ("⭐", "star"),
    ("🌟", "glowing star"),
    ("🔥", "fire lit hot"),
    ("⚡", "zap lightning high voltage"),
    ("❄️", "snowflake cold"),
    ("✅", "check done yes tick"),
    ("☑️", "ballot check"),
    ("✔️", "check mark"),
    ("❌", "cross x no wrong"),
    ("❎", "cross mark button"),
    ("➕", "plus add"),
    ("➖", "minus"),
    ("❓", "question"),
    ("❗", "exclamation"),
    ("‼️", "double exclamation"),
    ("⁉️", "exclamation question interrobang"),
    ("⚠️", "warning caution"),
    ("🚫", "prohibited no forbidden"),
    ("⛔", "no entry stop"),
    ("🔞", "eighteen adult nsfw"),
    ("♻️", "recycle"),
    ("🆗", "ok button"),
    ("🆕", "new button"),
    ("🆒", "cool button"),
    ("🔴", "red circle"),
    ("🟢", "green circle"),
    ("🔵", "blue circle"),
    ("⚫", "black circle"),
    ("⚪", "white circle"),
    // Hands and people
    ("👍", "thumbs up yes like good +1"),
    ("👎", "thumbs down no dislike bad -1"),
    ("👌", "ok perfect"),
    ("🤌", "pinched fingers italian"),
    ("🤏", "pinch small tiny"),
    ("✌️", "peace victory"),
    ("🤞", "fingers crossed luck hope"),
    ("🤟", "love you hand"),
    ("🤘", "rock on horns metal"),
    ("🤙", "call me shaka hang loose"),
    ("👈", "point left"),
    ("👉", "point right"),
    ("👆", "point up"),
    ("👇", "point down"),
    ("☝️", "index up one"),
    ("✋", "raised hand stop high five"),
    ("🖐️", "hand fingers splayed"),
    ("🖖", "vulcan spock"),
    ("👋", "wave hello hi bye"),
    ("👏", "clap applause bravo"),
    ("🙌", "raising hands hooray praise"),
    ("👐", "open hands"),
    ("🤲", "palms up together"),
    ("🤝", "handshake deal agree"),
    ("🙏", "pray please thanks folded hands"),
    ("✍️", "writing hand"),
    ("💪", "muscle strong flex"),
    ("🖕", "middle finger"),
    ("👀", "eyes look see"),
    ("👁️", "eye"),
    ("🧠", "brain smart"),
    ("👅", "tongue"),
    ("👄", "mouth lips"),
    ("💋", "kiss lips"),
    ("🫡", "salute"),
    ("🫠", "melting"),
    ("🫶", "heart hands"),
    ("🤷", "shrug dunno whatever"),
    ("🤦", "facepalm"),
    ("🙋", "raising hand me"),
    ("🙅", "no gesture"),
    ("🙆", "ok gesture"),
    ("💁", "tipping hand info"),
    ("🙇", "bow sorry"),
    ("🕺", "dance man disco"),
    ("💃", "dance woman"),
    ("🏃", "run running"),
    ("🚶", "walk walking"),
    ("👶", "baby"),
    ("🧑‍💻", "technologist coder hacker laptop"),
    ("🕵️", "detective spy"),
    ("🥷", "ninja stealth"),
    ("👑", "crown king queen"),
    // Animals and nature
    ("🐱", "cat kitty"),
    ("🐈", "cat"),
    ("🐈‍⬛", "black cat"),
    ("🐶", "dog puppy"),
    ("🐕", "dog"),
    ("🦊", "fox"),
    ("🐭", "mouse"),
    ("🐹", "hamster"),
    ("🐰", "rabbit bunny"),
    ("🐻", "bear"),
    ("🐼", "panda"),
    ("🐨", "koala"),
    ("🐯", "tiger"),
    ("🦁", "lion"),
    ("🐮", "cow"),
    ("🐷", "pig"),
    ("🐸", "frog"),
    ("🐵", "monkey"),
    ("🐔", "chicken"),
    ("🐧", "penguin"),
    ("🐦", "bird"),
    ("🦆", "duck"),
    ("🦉", "owl"),
    ("🦇", "bat"),
    ("🐺", "wolf"),
    ("🐴", "horse"),
    ("🦄", "unicorn"),
    ("🐝", "bee"),
    ("🐛", "bug caterpillar"),
    ("🦋", "butterfly"),
    ("🐌", "snail slow"),
    ("🐞", "ladybug"),
    ("🕷️", "spider"),
    ("🐢", "turtle slow"),
    ("🐍", "snake"),
    ("🦎", "lizard"),
    ("🐙", "octopus"),
    ("🦀", "crab rust"),
    ("🐟", "fish"),
    ("🐬", "dolphin"),
    ("🐳", "whale"),
    ("🦈", "shark"),
    ("🐊", "crocodile"),
    ("🐘", "elephant"),
    ("🦒", "giraffe"),
    ("🐾", "paw prints"),
    ("🌸", "cherry blossom flower"),
    ("🌹", "rose flower"),
    ("🌻", "sunflower"),
    ("🌷", "tulip"),
    ("🌱", "seedling plant"),
    ("🌲", "evergreen tree"),
    ("🌴", "palm tree"),
    ("🍀", "four leaf clover luck"),
    ("🍁", "maple leaf autumn"),
    ("🍄", "mushroom"),
    ("🌍", "earth globe world"),
    ("🌙", "moon night"),
    ("☀️", "sun sunny"),
    ("⛅", "cloud sun"),
    ("🌧️", "rain"),
    ("⛈️", "storm thunder"),
    ("🌈", "rainbow"),
    ("☔", "umbrella rain"),
    ("🌊", "wave ocean sea"),
    // Food and drink
    ("🍎", "apple"),
    ("🍊", "orange tangerine"),
    ("🍋", "lemon"),
    ("🍌", "banana"),
    ("🍉", "watermelon"),
    ("🍇", "grapes"),
    ("🍓", "strawberry"),
    ("🍒", "cherries"),
    ("🍑", "peach"),
    ("🥭", "mango"),
    ("🍍", "pineapple"),
    ("🥥", "coconut"),
    ("🥑", "avocado"),
    ("🍆", "eggplant aubergine"),
    ("🥕", "carrot"),
    ("🌽", "corn"),
    ("🌶️", "hot pepper chili spicy"),
    ("🥔", "potato"),
    ("🍞", "bread"),
    ("🧀", "cheese"),
    ("🥚", "egg"),
    ("🍳", "cooking fried egg"),
    ("🥓", "bacon"),
    ("🍔", "burger hamburger"),
    ("🍟", "fries"),
    ("🍕", "pizza"),
    ("🌭", "hot dog"),
    ("🌮", "taco"),
    ("🌯", "burrito"),
    ("🍜", "noodles ramen"),
    ("🍣", "sushi"),
    ("🍚", "rice"),
    ("🍿", "popcorn"),
    ("🍩", "donut doughnut"),
    ("🍪", "cookie"),
    ("🎂", "birthday cake"),
    ("🍰", "cake shortcake"),
    ("🧁", "cupcake"),
    ("🍫", "chocolate"),
    ("🍬", "candy sweet"),
    ("🍭", "lollipop"),
    ("🍦", "ice cream"),
    ("☕", "coffee tea hot"),
    ("🍵", "tea"),
    ("🧃", "juice box"),
    ("🥤", "soda cup drink"),
    ("🧋", "bubble tea boba"),
    ("🍺", "beer"),
    ("🍻", "beers cheers"),
    ("🥂", "champagne cheers toast"),
    ("🍷", "wine"),
    ("🥃", "whiskey tumbler"),
    ("🍸", "cocktail martini"),
    // Activities and objects
    ("🎉", "tada party celebrate hooray"),
    ("🎊", "confetti party"),
    ("🎈", "balloon party"),
    ("🎁", "gift present"),
    ("🏆", "trophy win champion"),
    ("🥇", "gold medal first"),
    ("⚽", "soccer football"),
    ("🏀", "basketball"),
    ("🎮", "video game controller gaming"),
    ("🕹️", "joystick arcade"),
    ("🎲", "dice game"),
    ("♟️", "chess pawn"),
    ("🎯", "bullseye target dart"),
    ("🎵", "music note"),
    ("🎶", "music notes"),
    ("🎤", "microphone sing karaoke"),
    ("🎧", "headphones music"),
    ("🎸", "guitar"),
    ("🎨", "art palette paint"),
    ("🎬", "clapper movie film"),
    ("📷", "camera photo"),
    ("📸", "camera flash"),
    ("📺", "tv television"),
    ("💻", "laptop computer"),
    ("🖥️", "desktop computer"),
    ("⌨️", "keyboard"),
    ("🖱️", "mouse computer"),
    ("📱", "phone mobile"),
    ("☎️", "telephone"),
    ("🔋", "battery"),
    ("🔌", "plug electric"),
    ("💡", "bulb idea"),
    ("🔦", "flashlight torch"),
    ("📚", "books library"),
    ("📖", "book open read"),
    ("📝", "memo note write"),
    ("✏️", "pencil"),
    ("📌", "pin pushpin"),
    ("📎", "paperclip attach"),
    ("✂️", "scissors cut"),
    ("📅", "calendar date"),
    ("📈", "chart up increase"),
    ("📉", "chart down decrease"),
    ("📊", "bar chart stats"),
    ("📦", "package box"),
    ("📫", "mailbox mail"),
    ("✉️", "envelope email letter"),
    ("🔒", "lock locked secure private"),
    ("🔓", "unlock unlocked open"),
    ("🔑", "key"),
    ("🗝️", "old key"),
    ("🔨", "hammer"),
    ("🛠️", "tools hammer wrench"),
    ("🔧", "wrench fix"),
    ("⚙️", "gear settings"),
    ("🧲", "magnet"),
    ("🧪", "test tube experiment"),
    ("🔬", "microscope science"),
    ("🔭", "telescope"),
    ("💊", "pill medicine"),
    ("💉", "syringe needle"),
    ("🧸", "teddy bear"),
    ("💰", "money bag"),
    ("💸", "money flying wings"),
    ("💳", "credit card"),
    ("💎", "gem diamond"),
    ("⏰", "alarm clock"),
    ("⌛", "hourglass time"),
    ("⏳", "hourglass flowing"),
    ("🔔", "bell notification"),
    ("🔕", "bell off mute"),
    ("📢", "loudspeaker announcement"),
    ("🗑️", "trash bin wastebasket"),
    ("🚀", "rocket launch ship it"),
    ("✈️", "airplane plane travel"),
    ("🚗", "car"),
    ("🚲", "bicycle bike"),
    ("🚂", "train locomotive"),
    ("⛵", "sailboat boat"),
    ("🏠", "house home"),
    ("🏰", "castle"),
    ("⛺", "tent camping"),
    ("🗺️", "map world"),
    ("🧭", "compass"),
    ("🏳️‍🌈", "rainbow flag pride"),
    ("🏴‍☠️", "pirate flag"),
    ("🚩", "red flag"),
    ("🏁", "chequered flag finish"),
];

/// Common shortcode names (as Slack, GitHub and Element use them) where
/// they differ from the first search word in [`ALL`].
const ALIASES: &[(&str, &str)] = &[
    ("smile", "😄"), ("smiley", "😃"), ("grinning", "😀"), ("grin", "😁"), ("laughing", "😆"), ("satisfied", "😆"),
    ("joy", "😂"), ("rofl", "🤣"), ("wink", "😉"), ("blush", "😊"), ("innocent", "😇"), ("heart_eyes", "😍"),
    ("kissing_heart", "😘"), ("yum", "😋"), ("stuck_out_tongue", "😛"), ("thinking", "🤔"), ("neutral_face", "😐"),
    ("smirk", "😏"), ("unamused", "😒"), ("roll_eyes", "🙄"), ("relieved", "😌"), ("pensive", "😔"), ("sleepy", "😪"),
    ("sleeping", "😴"), ("mask", "😷"), ("sunglasses", "😎"), ("nerd", "🤓"), ("confused", "😕"), ("worried", "😟"),
    ("open_mouth", "😮"), ("astonished", "😲"), ("flushed", "😳"), ("pleading", "🥺"), ("fearful", "😨"),
    ("cry", "😢"), ("sob", "😭"), ("scream", "😱"), ("disappointed", "😞"), ("sweat_smile", "😅"), ("sweat", "😓"),
    ("weary", "😩"), ("tired_face", "😫"), ("yawning", "🥱"), ("triumph", "😤"), ("rage", "😡"), ("angry", "😠"),
    ("upside_down", "🙃"), ("slightly_smiling_face", "🙂"), ("hugs", "🤗"), ("shushing", "🤫"), ("zany", "🤪"),
    ("partying", "🥳"), ("exploding_head", "🤯"), ("skull", "💀"), ("poop", "💩"), ("hankey", "💩"), ("clown", "🤡"),
    ("ghost", "👻"), ("robot", "🤖"), ("heart", "❤️"), ("broken_heart", "💔"), ("two_hearts", "💕"), ("sparkling_heart", "💖"),
    ("100", "💯"), ("boom", "💥"), ("sparkles", "✨"), ("star", "⭐"), ("star2", "🌟"), ("fire", "🔥"), ("zap", "⚡"),
    ("white_check_mark", "✅"), ("heavy_check_mark", "✔️"), ("check", "✅"), ("x", "❌"), ("warning", "⚠️"),
    ("question", "❓"), ("exclamation", "❗"), ("+1", "👍"), ("thumbsup", "👍"), ("-1", "👎"), ("thumbsdown", "👎"),
    ("ok_hand", "👌"), ("v", "✌️"), ("crossed_fingers", "🤞"), ("metal", "🤘"), ("call_me", "🤙"), ("wave", "👋"),
    ("clap", "👏"), ("raised_hands", "🙌"), ("handshake", "🤝"), ("pray", "🙏"), ("muscle", "💪"), ("eyes", "👀"),
    ("brain", "🧠"), ("shrug", "🤷"), ("facepalm", "🤦"), ("salute", "🫡"), ("cat", "🐱"), ("dog", "🐶"), ("fox", "🦊"),
    ("crab", "🦀"), ("tada", "🎉"), ("confetti_ball", "🎊"), ("balloon", "🎈"), ("gift", "🎁"), ("trophy", "🏆"),
    ("rocket", "🚀"), ("coffee", "☕"), ("beer", "🍺"), ("beers", "🍻"), ("pizza", "🍕"), ("cake", "🎂"),
    ("lock", "🔒"), ("unlock", "🔓"), ("key", "🔑"), ("bulb", "💡"), ("bell", "🔔"), ("pushpin", "📌"), ("memo", "📝"),
    ("eggplant", "🍆"), ("peach", "🍑"), ("see_no_evil", "🙈"), ("hear_no_evil", "🙉"), ("speak_no_evil", "🙊"),
];

/// The emoji for a shortcode name like `smile` or `thumbsup` (without colons).
pub fn shortcode(name: &str) -> Option<&'static str> {
    let name = name.to_lowercase();
    if let Some((_, e)) = ALIASES.iter().find(|(n, _)| *n == name) {
        return Some(e);
    }
    let words = name.replace(['_', '-'], " ");
    // The emoji whose first word is the name, else any whose words are.
    ALL.iter()
        .find(|(_, k)| k.split(' ').next() == Some(words.as_str()))
        .or_else(|| ALL.iter().find(|(_, k)| k.contains(words.as_str()) && k.split(' ').any(|w| w == words)))
        .map(|(e, _)| *e)
}

/// The name shown for an emoji in suggestions (`:smile:`).
pub fn name_of(emoji: &str) -> &'static str {
    ALIASES
        .iter()
        .find(|(_, e)| *e == emoji)
        .map(|(n, _)| *n)
        .or_else(|| ALL.iter().find(|(e, _)| *e == emoji).and_then(|(_, k)| k.split(' ').next()))
        .unwrap_or_default()
}

/// Emoji for a half-typed shortcode: the exact name first, then names
/// starting with it, then other search matches.
pub fn suggest(q: &str) -> Vec<&'static str> {
    let q = q.to_lowercase();
    let mut out: Vec<&'static str> = Vec::new();
    let mut add = |e: &'static str| {
        if !out.contains(&e) {
            out.push(e);
        }
    };
    if let Some(e) = shortcode(&q) {
        add(e);
    }
    for (n, e) in ALIASES {
        if n.starts_with(q.as_str()) {
            add(e);
        }
    }
    for e in search(&q.replace(['_', '-'], " ")) {
        add(e);
    }
    out
}

/// Replaces `:name:` shortcodes in `text` with their emoji (unknown names
/// stay as typed; code isn't touched).
pub fn replace_shortcodes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_code = false;
    for (n, line) in text.split('\n').enumerate() {
        if n > 0 {
            out.push('\n');
        }
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            out.push_str(line);
            continue;
        }
        if in_code {
            out.push_str(line);
            continue;
        }
        let mut rest = line;
        while let Some(a) = rest.find(':') {
            let after = &rest[a + 1..];
            let name_len = after.find(|c: char| !(c.is_alphanumeric() || "_+-".contains(c))).unwrap_or(after.len());
            let emoji = (name_len > 0 && after[name_len..].starts_with(':')).then(|| shortcode(&after[..name_len])).flatten();
            match emoji {
                Some(e) => {
                    out.push_str(&rest[..a]);
                    out.push_str(e);
                    rest = &after[name_len + 1..];
                }
                None => {
                    out.push_str(&rest[..a + 1]);
                    rest = after;
                }
            }
        }
        out.push_str(rest);
    }
    out
}

/// Also turns a whole word `:name` (no closing colon) into its emoji, for
/// names that match exactly. Used when sending.
pub fn replace_open_shortcodes(text: &str) -> String {
    let text = replace_shortcodes(text);
    let mut out = String::with_capacity(text.len());
    for (i, word) in text.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let name = word.strip_prefix(':').filter(|n| n.chars().count() >= 2 && n.chars().all(|c| c.is_alphanumeric() || "_+-".contains(c)));
        match name.and_then(shortcode) {
            Some(e) => out.push_str(e),
            None => out.push_str(word),
        }
    }
    out
}

/// Emoji whose words contain every word of `query` (all of them for an empty query).
pub fn search(query: &str) -> Vec<&'static str> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    ALL.iter()
        .filter(|(e, k)| words.iter().all(|w| k.split(' ').any(|x| x.starts_with(w.as_str())) || *e == w))
        .map(|(e, _)| *e)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_by_word_start() {
        assert!(search("thumb").contains(&"👍"));
        assert!(search("cat heart").contains(&"😻"));
        assert!(!search("cat heart").contains(&"🐱"));
        assert_eq!(search("").len(), ALL.len());
        assert!(search("zzzznothing").is_empty());
    }

    #[test]
    fn shortcodes() {
        assert_eq!(shortcode("smile"), Some("😄"));
        assert_eq!(shortcode("thumbsup"), Some("👍"));
        assert_eq!(shortcode("+1"), Some("👍"));
        assert_eq!(shortcode("cat"), Some("🐱"));
        assert_eq!(shortcode("heart_eyes"), Some("😍"));
        assert_eq!(shortcode("nonsense"), None);
        assert_eq!(suggest("smi")[0], "😄");
        assert_eq!(name_of("😄"), "smile");
        assert_eq!(replace_shortcodes("hi :smile: and :tada:!"), "hi 😄 and 🎉!");
        assert_eq!(replace_shortcodes("time 10:30:00 :nope: a:b"), "time 10:30:00 :nope: a:b");
        assert_eq!(replace_shortcodes("```\n:smile:\n```\n:smile:"), "```\n:smile:\n```\n😄");
        assert_eq!(replace_open_shortcodes("nice :smile and :tada: :zzzq"), "nice 😄 and 🎉 :zzzq");
    }

    #[test]
    fn every_emoji_has_a_picture() {
        let missing: Vec<&str> = ALL.iter().map(|(e, _)| *e).filter(|e| picture(e).is_none()).collect();
        assert!(missing.is_empty(), "no picture for {missing:?}");
        assert!(picture("a").is_none());
        for (_, png) in pictures::PICTURES {
            assert!(png.starts_with(b"\x89PNG"));
        }
    }

    #[test]
    fn emoji_only_messages() {
        assert_eq!(only_emoji(" 😀😂 ", 8), Some(vec!["😀", "😂"]));
        assert_eq!(only_emoji("❤️ 👍", 8), Some(vec!["❤️", "👍"]));
        assert_eq!(only_emoji("❤", 8), Some(vec!["❤️"]));
        assert_eq!(only_emoji("🧑‍💻", 8), Some(vec!["🧑‍💻"]));
        assert_eq!(only_emoji("hi 😀", 8), None);
        assert_eq!(only_emoji("", 8), None);
        assert_eq!(only_emoji("😀😀😀", 2), None);
    }

    #[test]
    fn no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for (e, _) in ALL {
            assert!(seen.insert(*e), "{e} listed twice");
        }
        for q in QUICK {
            assert!(ALL.iter().any(|(e, _)| *e == q));
        }
    }
}
