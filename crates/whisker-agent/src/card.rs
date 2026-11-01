//! Character cards (Tavern / SillyTavern format): reading and writing
//! CCv1, CCv2 and CCv3, as JSON files or embedded in PNG pictures.
//!
//! A PNG card is a normal picture with the card in a text chunk:
//! base64 JSON under the keyword `ccv3` (v3) or `chara` (v2, also used by
//! v1 tools). We read v3 first, then v2. We write both, as most apps do.
//!
//! [`Card::raw`] keeps the whole original `data` object, so fields this app
//! doesn't use (extensions, assets, …) survive an import and export.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Card {
    pub name: String,
    pub description: String,
    pub personality: String,
    pub scenario: String,
    pub first_mes: String,
    pub mes_example: String,
    pub alternate_greetings: Vec<String>,
    pub system_prompt: String,
    pub post_history_instructions: String,
    pub creator_notes: String,
    pub creator: String,
    pub character_version: String,
    pub tags: Vec<String>,
    pub nickname: String,
    pub character_book: Option<Book>,
    /// The card's full `data` object as imported (unknown fields included).
    pub raw: Map<String, Value>,
}

/// A lorebook: entries whose text is added when their keywords come up.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Book {
    pub name: String,
    /// How many recent messages are searched for keywords.
    pub scan_depth: Option<usize>,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Entry {
    pub keys: Vec<String>,
    pub secondary_keys: Vec<String>,
    pub content: String,
    pub enabled: bool,
    /// Always included, keywords or not.
    pub constant: bool,
    /// Needs a key *and* a secondary key.
    pub selective: bool,
    pub case_sensitive: bool,
    pub insertion_order: i64,
    pub name: String,
    pub comment: String,
    /// CCv3: `before_char` or `after_char` (some tools write a number).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<Value>,
    /// SillyTavern keeps `position`, `depth` and `role` here.
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub extensions: Map<String, Value>,
}

impl Default for Entry {
    fn default() -> Self {
        Self {
            keys: Vec::new(),
            secondary_keys: Vec::new(),
            content: String::new(),
            enabled: true,
            constant: false,
            selective: false,
            case_sensitive: false,
            insertion_order: 100,
            name: String::new(),
            comment: String::new(),
            position: None,
            extensions: Map::new(),
        }
    }
}

/// Vault extra holding the picture of `account`'s card (base64 PNG), kept
/// apart from the agent settings so they stay small.
pub fn picture_key(account: whisker_core::AccountId) -> String {
    format!("card_picture:{account}")
}

/// An imported card and, for PNG cards, the picture it came in.
pub struct Imported {
    pub card: Card,
    pub png: Option<Vec<u8>>,
}

/// Reads a card from a `.json` or `.png` file's bytes.
pub fn import(bytes: &[u8]) -> Result<Imported> {
    if bytes.starts_with(PNG_SIGNATURE) {
        let text = png_card_text(bytes)?;
        let json = decode_base64(&text).context("the picture's card data isn't valid base64")?;
        let v: Value = serde_json::from_slice(&json).context("the picture's card data isn't valid JSON")?;
        return Ok(Imported { card: Card::from_json(v)?, png: Some(bytes.to_vec()) });
    }
    let text = std::str::from_utf8(bytes).context("not a PNG picture or a JSON file")?;
    let v: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).context("not valid JSON")?;
    Ok(Imported { card: Card::from_json(v)?, png: None })
}

impl Card {
    /// From any card JSON: `{spec, data}` (v2/v3) or the flat v1 shape.
    pub fn from_json(v: Value) -> Result<Self> {
        let Value::Object(top) = v else { bail!("a character card is a JSON object") };
        let data = match top.get("data") {
            Some(Value::Object(d)) if top.contains_key("spec") || d.contains_key("name") => d.clone(),
            _ => top,
        };
        let mut card: Card = serde_json::from_value(Value::Object(data.clone())).context("unexpected field types in the card")?;
        if card.name.trim().is_empty() {
            // v1 cards sometimes use `char_name`, and `char_persona` etc.
            card.name = str_field(&data, "char_name");
        }
        if card.name.trim().is_empty() {
            bail!("this card has no character name");
        }
        for (field, alt) in [("description", "char_persona"), ("scenario", "world_scenario"), ("first_mes", "char_greeting"), ("mes_example", "example_dialogue")] {
            let target = match field {
                "description" => &mut card.description,
                "scenario" => &mut card.scenario,
                "first_mes" => &mut card.first_mes,
                _ => &mut card.mes_example,
            };
            if target.is_empty() {
                *target = str_field(&data, alt);
            }
        }
        card.raw = data;
        Ok(card)
    }

    /// The card as CCv3 JSON (`spec: chara_card_v3`), keeping unknown fields.
    pub fn to_v3(&self) -> Value {
        let mut data = self.raw.clone();
        let mine = serde_json::to_value(self).expect("cards serialize");
        if let Value::Object(fields) = mine {
            for (k, v) in fields {
                if k != "raw" {
                    data.insert(k, v);
                }
            }
        }
        if data.get("character_book").is_some_and(Value::is_null) {
            data.remove("character_book");
        }
        for (k, v) in [("extensions", json!({})), ("group_only_greetings", json!([])), ("creator_notes", json!(""))] {
            data.entry(k).or_insert(v);
        }
        json!({ "spec": "chara_card_v3", "spec_version": "3.0", "data": data })
    }

    /// The same card as CCv2 JSON, for apps that only read v2.
    pub fn to_v2(&self) -> Value {
        let mut v = self.to_v3();
        v["spec"] = json!("chara_card_v2");
        v["spec_version"] = json!("2.0");
        v
    }

    /// `text` with `{{char}}` and `{{user}}` (and the old `<BOT>`/`<USER>`) filled in.
    pub fn fill(&self, text: &str, user: &str) -> String {
        let char_name = if self.nickname.trim().is_empty() { self.name.as_str() } else { self.nickname.as_str() };
        let mut out = text.to_owned();
        for (pat, with) in [("{{char}}", char_name), ("<BOT>", char_name), ("{{user}}", user), ("<USER>", user)] {
            out = replace_ignore_case(&out, pat, with);
        }
        out
    }

    /// The character, written out for the system prompt. `base` is the
    /// owner's own persona text, which a card's system prompt can include
    /// with `{{original}}`.
    pub fn system(&self, base: &str, user: &str) -> String {
        self.system_with(base, user, "")
    }

    /// [`Card::system`] with lorebook text placed ahead of the character.
    pub fn system_with(&self, base: &str, user: &str, before: &str) -> String {
        let mut out = String::new();
        let sys = self.system_prompt.trim();
        if sys.is_empty() {
            out.push_str(base.trim());
        } else {
            out.push_str(&replace_ignore_case(sys, "{{original}}", base.trim()));
        }
        out.push_str(&format!("\n\nYou are playing {}. Stay in character.", self.name));
        if !before.trim().is_empty() {
            out.push_str(&format!("\n\n{}", before.trim()));
        }
        for (label, text) in [
            ("Character", &self.description),
            ("Personality", &self.personality),
            ("Scenario", &self.scenario),
            ("Example messages (style only, not things that happened)", &self.mes_example),
        ] {
            if !text.trim().is_empty() {
                out.push_str(&format!("\n\n{label}:\n{}", text.trim()));
            }
        }
        self.fill(out.trim(), user)
    }

    /// Lorebook text for what's being talked about: constant entries, and
    /// those whose keywords appear in `recent` (newest last), placed where
    /// each entry asks (see [`Lore`]).
    pub fn lore(&self, recent: &[&str], user: &str) -> Lore {
        let mut out = Lore::default();
        let Some(book) = &self.character_book else { return out };
        let scanned = |depth: usize| -> Vec<&str> { recent.iter().rev().take(depth.max(1)).copied().collect() };
        let hit = |keys: &[String], case: bool, scan: &[&str]| {
            keys.iter().map(|k| k.trim()).filter(|k| !k.is_empty()).any(|k| {
                scan.iter().any(|t| if case { t.contains(k) } else { t.to_lowercase().contains(&k.to_lowercase()) })
            })
        };
        let mut picked: Vec<(&Entry, Placed, String)> = Vec::new();
        for e in book.entries.iter().filter(|e| e.enabled) {
            let (d, text) = decorators(&e.content);
            if text.trim().is_empty() || d.dont_activate {
                continue;
            }
            let scan = scanned(d.scan_depth.or(book.scan_depth).unwrap_or(4));
            let on = e.constant
                || d.activate
                || (hit(&e.keys, e.case_sensitive, &scan) && (!e.selective || e.secondary_keys.is_empty() || hit(&e.secondary_keys, e.case_sensitive, &scan)));
            if on {
                picked.push((e, placement(e, &d), self.fill(text.trim(), user)));
            }
        }
        picked.sort_by_key(|(e, _, _)| e.insertion_order);
        let join = |v: Vec<&str>| v.join("\n\n");
        out.before = join(picked.iter().filter(|p| matches!(p.1, Placed::Before)).map(|p| p.2.as_str()).collect());
        out.after = join(picked.iter().filter(|p| matches!(p.1, Placed::After)).map(|p| p.2.as_str()).collect());
        let mut notes: Vec<(usize, Role, &str)> = picked.iter().filter_map(|p| match p.1 { Placed::Depth(n, r) => Some((n, r, p.2.as_str())), _ => None }).collect();
        // Deepest first, so depth 0 ends up last (sort is stable: order kept within a depth).
        notes.sort_by_key(|n| std::cmp::Reverse(n.0));
        out.notes = notes.into_iter().map(|(depth, role, text)| Note { depth, role, text: text.to_owned() }).collect();
        out
    }

    /// Writes the card into a PNG picture (replacing any card already in it).
    pub fn to_png(&self, picture: &[u8]) -> Result<Vec<u8>> {
        if !picture.starts_with(PNG_SIGNATURE) {
            bail!("the picture must be a PNG");
        }
        let mut out = PNG_SIGNATURE.to_vec();
        let mut wrote = false;
        for chunk in chunks(picture)? {
            if chunk.kind == *b"tEXt" && matches!(text_keyword(chunk.data), Some("chara" | "ccv3")) {
                continue;
            }
            if chunk.kind == *b"IEND" {
                for (key, v) in [("chara", self.to_v2()), ("ccv3", self.to_v3())] {
                    let mut data = key.as_bytes().to_vec();
                    data.push(0);
                    data.extend(encode_base64(v.to_string().as_bytes()).bytes());
                    write_chunk(&mut out, b"tEXt", &data);
                }
                wrote = true;
            }
            write_chunk(&mut out, &chunk.kind, chunk.data);
        }
        if !wrote {
            bail!("that PNG is cut short");
        }
        Ok(out)
    }
}

/// Lorebook entries for one reply, by where they go.
#[derive(Debug, Default, PartialEq)]
pub struct Lore {
    /// Ahead of the character description (`before_char`, `@@position before_desc`).
    pub before: String,
    /// After the character, as world info (the default).
    pub after: String,
    /// At a depth in the chat (`@@depth N`, SillyTavern's "at depth"),
    /// deepest first: these go at the end of the conversation, where
    /// reminders actually work, not up in the system prompt.
    pub notes: Vec<Note>,
}

#[derive(Debug, PartialEq)]
pub struct Note {
    pub depth: usize,
    pub role: Role,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Copy)]
enum Placed {
    Before,
    After,
    Depth(usize, Role),
}

/// An entry's CCv3 decorators (`@@name value` lines at the top of its text).
#[derive(Debug, Default)]
struct Decor {
    depth: Option<usize>,
    role: Option<Role>,
    position: Option<String>,
    activate: bool,
    dont_activate: bool,
    scan_depth: Option<usize>,
}

fn role(s: &str) -> Option<Role> {
    match s.trim().to_lowercase().as_str() {
        "system" | "0" => Some(Role::System),
        "user" | "1" => Some(Role::User),
        "assistant" | "model" | "2" => Some(Role::Assistant),
        _ => None,
    }
}

/// Splits the decorators off an entry's text. Every `@@…` line is taken
/// out (`@@@` fallbacks and ones this app doesn't use too), so none reach
/// the model as prose.
fn decorators(content: &str) -> (Decor, String) {
    let mut d = Decor::default();
    let mut text = Vec::new();
    for line in content.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("@@") else {
            text.push(line);
            continue;
        };
        if rest.starts_with('@') {
            continue;
        }
        let (name, value) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let value = value.trim();
        match name.to_lowercase().as_str() {
            "depth" => d.depth = value.parse().ok(),
            "role" => d.role = role(value),
            "position" => d.position = Some(value.to_lowercase()),
            "activate" => d.activate = true,
            "dont_activate" => d.dont_activate = true,
            "scan_depth" => d.scan_depth = value.parse().ok(),
            _ => {}
        }
    }
    (d, text.join("\n"))
}

/// Where an entry goes: decorators first, then SillyTavern's extensions
/// (position 0 before, 4 at depth), then the CCv3 `position` field.
fn placement(e: &Entry, d: &Decor) -> Placed {
    let ext = |k: &str| e.extensions.get(k);
    let ext_num = |k: &str| ext(k).and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()));
    let ext_role = ext("role").and_then(|v| role(&v.as_u64().map(|n| n.to_string()).or_else(|| v.as_str().map(str::to_owned))?));
    let role = d.role.or(ext_role).unwrap_or(Role::System);
    if let Some(depth) = d.depth {
        return Placed::Depth(depth, role);
    }
    match d.position.as_deref() {
        Some("before_desc" | "before_char") => return Placed::Before,
        Some(_) => return Placed::After,
        None => {}
    }
    match ext_num("position") {
        Some(0) => return Placed::Before,
        Some(4) => return Placed::Depth(ext_num("depth").unwrap_or(4) as usize, role),
        Some(_) => return Placed::After,
        None => {}
    }
    match &e.position {
        Some(Value::String(p)) if p == "before_char" => Placed::Before,
        Some(Value::Number(n)) if n.as_u64() == Some(0) => Placed::Before,
        _ => Placed::After,
    }
}

fn str_field(m: &Map<String, Value>, k: &str) -> String {
    m.get(k).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn replace_ignore_case(text: &str, pat: &str, with: &str) -> String {
    let lower = text.to_lowercase();
    let pat_l = pat.to_lowercase();
    // Lower-casing can change byte lengths outside ASCII; fall back to exact matching then.
    if lower.len() != text.len() {
        return text.replace(pat, with);
    }
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(at) = lower[i..].find(&pat_l) {
        out.push_str(&text[i..i + at]);
        out.push_str(with);
        i += at + pat.len();
    }
    out.push_str(&text[i..]);
    out
}

struct Chunk<'a> {
    kind: [u8; 4],
    data: &'a [u8],
}

fn chunks(png: &[u8]) -> Result<Vec<Chunk<'_>>> {
    let mut out = Vec::new();
    let mut i = PNG_SIGNATURE.len();
    while i + 12 <= png.len() {
        let len = u32::from_be_bytes(png[i..i + 4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = png[i + 4..i + 8].try_into().unwrap();
        let end = i.checked_add(12 + len).filter(|e| *e <= png.len()).ok_or_else(|| anyhow!("the PNG is damaged"))?;
        out.push(Chunk { kind, data: &png[i + 8..i + 8 + len] });
        i = end;
        if &kind == b"IEND" {
            break;
        }
    }
    Ok(out)
}

fn text_keyword(data: &[u8]) -> Option<&str> {
    let nul = data.iter().position(|b| *b == 0)?;
    std::str::from_utf8(&data[..nul]).ok()
}

/// The card text in a PNG: `ccv3` if present, else `chara`.
fn png_card_text(png: &[u8]) -> Result<String> {
    let mut found: Option<(bool, String)> = None;
    for c in chunks(png)? {
        let (key, text) = match &c.kind {
            b"tEXt" => {
                let Some(key) = text_keyword(c.data) else { continue };
                (key, String::from_utf8_lossy(&c.data[key.len() + 1..]).into_owned())
            }
            // iTXt: keyword \0 compression-flag method lang \0 translated \0 text
            b"iTXt" => {
                let Some(key) = text_keyword(c.data) else { continue };
                let rest = &c.data[key.len() + 1..];
                if rest.first() != Some(&0) {
                    continue; // compressed: no card tool writes these
                }
                let mut parts = rest[2..].splitn(3, |b| *b == 0);
                let (_, _, Some(text)) = (parts.next(), parts.next(), parts.next()) else { continue };
                (key, String::from_utf8_lossy(text).into_owned())
            }
            _ => continue,
        };
        match key.to_ascii_lowercase().as_str() {
            "ccv3" => found = Some((true, text)),
            "chara" if found.is_none() => found = Some((false, text)),
            _ => {}
        }
    }
    found.map(|f| f.1).ok_or_else(|| anyhow!("this picture has no character card in it"))
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    out.extend(kind);
    out.extend(data);
    let mut crc = Crc::new();
    crc.update(kind);
    crc.update(data);
    out.extend(crc.finish().to_be_bytes());
}

struct Crc(u32);

impl Crc {
    fn new() -> Self {
        Crc(0xffff_ffff)
    }
    fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u32;
            for _ in 0..8 {
                self.0 = if self.0 & 1 != 0 { 0xedb8_8320 ^ (self.0 >> 1) } else { self.0 >> 1 };
            }
        }
    }
    fn finish(self) -> u32 {
        !self.0
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode_base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            out.push(if i <= c.len() { B64[(n >> shift & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// Standard or URL-safe base64; whitespace and padding are ignored.
pub fn decode_base64(text: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for b in text.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => bail!("bad character in base64"),
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_png() -> Vec<u8> {
        // A 1×1 PNG: signature, IHDR, IDAT, IEND.
        let mut png = PNG_SIGNATURE.to_vec();
        write_chunk(&mut png, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        write_chunk(&mut png, b"IDAT", &[0x78, 0x9c, 0x63, 0x60, 0, 0, 0, 2, 0, 1]);
        write_chunk(&mut png, b"IEND", &[]);
        png
    }

    fn v3() -> Value {
        json!({
            "spec": "chara_card_v3", "spec_version": "3.0",
            "data": {
                "name": "Mira", "description": "{{char}} runs a tea shop.", "personality": "warm",
                "scenario": "", "first_mes": "Welcome, {{user}}!", "mes_example": "",
                "alternate_greetings": ["Hi again"], "tags": ["nsfw", "fantasy"],
                "system_prompt": "Narrate as {{char}}. {{original}}",
                "extensions": {"depth_prompt": {"depth": 4}},
                "character_book": {"scan_depth": 2, "entries": [
                    {"keys": ["Oolong"], "content": "Oolong is the shop cat.", "enabled": true, "insertion_order": 2},
                    {"keys": [], "content": "The shop is in Kyoto.", "enabled": true, "constant": true, "insertion_order": 1},
                    {"keys": ["tea"], "secondary_keys": ["price"], "selective": true, "content": "Tea costs 3 coins.", "enabled": true},
                    {"keys": ["off"], "content": "never", "enabled": false}
                ]}
            }
        })
    }

    #[test]
    fn reads_v3_json_and_keeps_unknown_fields() {
        let card = import(v3().to_string().as_bytes()).unwrap().card;
        assert_eq!(card.name, "Mira");
        assert_eq!(card.tags, ["nsfw", "fantasy"]);
        assert_eq!(card.alternate_greetings, ["Hi again"]);
        let out = card.to_v3();
        assert_eq!(out["spec"], "chara_card_v3");
        assert_eq!(out["spec_version"], "3.0");
        assert_eq!(out["data"]["extensions"]["depth_prompt"]["depth"], 4);
        assert_eq!(Card::from_json(out.clone()).unwrap().to_v3(), out);
    }

    #[test]
    fn reads_v1_and_v2() {
        let v1 = json!({"name": "Old", "description": "d", "first_mes": "hey", "personality": "", "scenario": "", "mes_example": ""});
        assert_eq!(Card::from_json(v1).unwrap().first_mes, "hey");
        let v1b = json!({"char_name": "Older", "char_persona": "p", "char_greeting": "yo"});
        let c = Card::from_json(v1b).unwrap();
        assert_eq!((c.name.as_str(), c.description.as_str(), c.first_mes.as_str()), ("Older", "p", "yo"));
        let v2 = json!({"spec": "chara_card_v2", "spec_version": "2.0", "data": {"name": "Two", "description": "x"}});
        assert_eq!(Card::from_json(v2).unwrap().name, "Two");
        assert!(Card::from_json(json!({"description": "no name"})).is_err());
        assert!(import(b"not json").is_err());
    }

    #[test]
    fn png_round_trip_prefers_v3() {
        let card = Card::from_json(v3()).unwrap();
        let png = card.to_png(&tiny_png()).unwrap();
        image_check(&png);
        let back = import(&png).unwrap();
        assert_eq!(back.card.to_v3(), card.to_v3());
        assert!(back.png.is_some());
        // A v2-only picture (what most sites serve).
        let mut v2png = PNG_SIGNATURE.to_vec();
        for c in chunks(&tiny_png()).unwrap() {
            if &c.kind == b"IEND" {
                let mut d = b"chara\0".to_vec();
                d.extend(encode_base64(json!({"spec": "chara_card_v2", "data": {"name": "Vee"}}).to_string().as_bytes()).bytes());
                write_chunk(&mut v2png, b"tEXt", &d);
            }
            write_chunk(&mut v2png, &c.kind, c.data);
        }
        assert_eq!(import(&v2png).unwrap().card.name, "Vee");
        // Re-exporting replaces the old card instead of adding a second one.
        let again = Card { name: "Mira 2".into(), ..card.clone() }.to_png(&png).unwrap();
        assert_eq!(chunks(&again).unwrap().iter().filter(|c| &c.kind == b"tEXt").count(), 2);
        assert_eq!(import(&again).unwrap().card.name, "Mira 2");
        assert!(import(&tiny_png()).is_err());
    }

    /// Chunk CRCs are right (a decoder would reject the file otherwise).
    fn image_check(png: &[u8]) {
        let mut i = 8;
        while i < png.len() {
            let len = u32::from_be_bytes(png[i..i + 4].try_into().unwrap()) as usize;
            let mut crc = Crc::new();
            crc.update(&png[i + 4..i + 8 + len]);
            assert_eq!(crc.finish().to_be_bytes(), png[i + 8 + len..i + 12 + len]);
            i += 12 + len;
        }
    }

    #[test]
    fn prompt_and_lorebook() {
        let card = Card::from_json(v3()).unwrap();
        let sys = card.system("Be brief.", "Sam");
        assert!(sys.starts_with("Narrate as Mira. Be brief."), "{sys}");
        assert!(sys.contains("Mira runs a tea shop."));
        assert!(!sys.contains("Scenario"));
        assert_eq!(card.fill(&card.first_mes, "Sam"), "Welcome, Sam!");
        let after = |t: &str| card.lore(&[t], "Sam").after;
        assert_eq!(after("hello"), "The shop is in Kyoto.");
        assert_eq!(after("where is oolong?"), "The shop is in Kyoto.\n\nOolong is the shop cat.");
        // Past the scan depth: not seen.
        assert_eq!(card.lore(&["oolong", "a", "b"], "Sam").after, "The shop is in Kyoto.");
        assert!(!after("tea please").contains("coins"));
        assert!(after("tea price?").contains("3 coins"));
        assert!(!after("off").contains("never"));
    }

    #[test]
    fn lore_decorators_place_entries() {
        let card = Card::from_json(json!({"spec": "chara_card_v3", "spec_version": "3.0", "data": {
            "name": "Mira",
            "character_book": {"entries": [
                {"keys": [], "constant": true, "insertion_order": 20,
                 "content": "@@depth 0\n@@role assistant\n@@position after_desc\n@@ignore_on_max_context\nEnd every reply with <state>.", "enabled": true},
                {"keys": [], "constant": true, "insertion_order": 5, "content": "@@position before_desc\nIt is raining.", "enabled": true},
                {"keys": [], "constant": true, "insertion_order": 30, "content": "Tea is cheap.", "enabled": true},
                {"keys": [], "constant": true, "insertion_order": 1, "content": "Deep note.", "enabled": true,
                 "extensions": {"position": 4, "depth": 3, "role": 1}},
                {"keys": [], "constant": true, "content": "@@dont_activate\nNever.", "enabled": true},
                {"keys": ["zzz"], "content": "@@activate\nAlways.", "enabled": true}
            ]}
        }}))
        .unwrap();
        let l = card.lore(&["hi"], "Sam");
        assert_eq!(l.before, "It is raining.");
        assert_eq!(l.after, "Tea is cheap.\n\nAlways.");
        assert_eq!(
            l.notes,
            [
                Note { depth: 3, role: Role::User, text: "Deep note.".into() },
                Note { depth: 0, role: Role::Assistant, text: "End every reply with <state>.".into() },
            ]
        );
        // SillyTavern's extensions survive a round trip.
        assert_eq!(Card::from_json(card.to_v3()).unwrap().character_book.unwrap().entries[3].extensions["depth"], 3);
    }

    #[test]
    fn base64_round_trip() {
        for s in ["", "a", "ab", "abc", "abcd", "héllo wörld ✨"] {
            assert_eq!(decode_base64(&encode_base64(s.as_bytes())).unwrap(), s.as_bytes());
        }
        assert_eq!(encode_base64(b"Man"), "TWFu");
        assert_eq!(decode_base64("TW\nFu").unwrap(), b"Man");
    }
}
