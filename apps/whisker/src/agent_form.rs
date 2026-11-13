//! Converts between an AI's settings page and its `AgentConfig`.

use crate::AgentForm;
use anyhow::{Context, Result, bail};
use matrix_sdk::ruma::{OwnedRoomId, OwnedUserId};
use slint::SharedString;
use std::collections::BTreeSet;
use whisker_agent::card::Card;
use whisker_agent::config::default_prompt;
use whisker_agent::{AgentConfig, ApiConfig, Detected, Permission, Trigger};
use whisker_core::AccountId;

fn join<T: AsRef<str>>(items: impl IntoIterator<Item = T>) -> SharedString {
    items.into_iter().map(|i| i.as_ref().to_owned()).collect::<Vec<_>>().join(", ").into()
}

fn list(s: &str) -> impl Iterator<Item = &str> {
    s.split([',', '\n']).map(str::trim).filter(|x| !x.is_empty())
}

fn users(s: &str) -> Result<BTreeSet<OwnedUserId>> {
    list(s).map(|u| u.try_into().with_context(|| format!("“{u}” isn't a user id like @name:server"))).collect()
}

fn rooms(s: &str) -> Result<BTreeSet<OwnedRoomId>> {
    list(s).map(|r| r.try_into().with_context(|| format!("“{r}” isn't a room id like !abc:server"))).collect()
}

/// The form for `account`'s AI, from its saved config (or defaults).
pub fn to_form(account: AccountId, name: &str, cfg: Option<&AgentConfig>) -> AgentForm {
    let Some(c) = cfg else {
        return AgentForm {
            account: account.to_string().into(),
            name: name.into(),
            exists: false,
            enabled: true,
            max_tokens: 1024,
            g_reply: true,
            min_level: 50,
            history: 40,
            summarize: true,
            context_tokens: 6000,
            yield_secs: 8,
            max_rapid: 4,
            cooldown: 2,
            max_steps: 6,
            system_prompt: default_prompt().into(),
            greet: true,
            ..Default::default()
        };
    };
    let g = &c.permissions.global;
    let has = |p| g.contains(&p);
    AgentForm {
        account: account.to_string().into(),
        name: name.into(),
        exists: true,
        enabled: c.enabled,
        url: c.api.url.as_str().into(),
        api_key_set: c.api.api_key.as_ref().is_some_and(|k| !k.is_empty()),
        api_key: "".into(),
        model: c.api.model.as_str().into(),
        service: c.api.service.as_str().into(),
        max_tokens: c.max_tokens.min(i32::MAX as u32) as i32,
        extra_json: if c.extra_params.is_empty() {
            "".into()
        } else {
            serde_json::to_string_pretty(&c.extra_params).unwrap_or_default().into()
        },
        trigger: match c.trigger {
            Trigger::Mention => 0,
            Trigger::Every => 1,
            Trigger::DirectOnly => 2,
        },
        system_prompt: c.system_prompt.as_str().into(),
        rooms: join(c.rooms.iter().map(|r| r.as_str())),
        history: c.history.min(200) as i32,
        summarize: c.summarize,
        summary_model: c.summary_model.as_str().into(),
        context_tokens: c.context_tokens.min(200_000) as i32,
        yield_secs: c.yield_secs.min(600) as i32,
        max_rapid: c.max_rapid_replies.min(50) as i32,
        venice_system_prompt: c.venice_system_prompt,
        venice_web_search: c.venice_web_search,
        see_images: c.see_images,
        image_model: c.image_model.as_str().into(),
        web: c.web,
        web_sites: c.web_sites.join(", ").into(),
        reply_chars: c.reply_chars.min(20_000) as i32,
        cooldown: c.cooldown_secs.min(3600) as i32,
        max_steps: c.max_steps.min(20) as i32,
        g_reply: has(Permission::Reply),
        g_redact: has(Permission::Redact),
        g_kick: has(Permission::Kick),
        g_ban: has(Permission::Ban),
        g_unban: has(Permission::Unban),
        g_invite: has(Permission::Invite),
        g_power: has(Permission::SetPowerLevel),
        commanders: join(c.permissions.commanders.users.iter().map(|u| u.as_str())),
        min_level: c.permissions.commanders.min_power_level.clamp(0, 100) as i32,
        autonomous: c.permissions.autonomous,
        max_grant: c.permissions.max_grant_level.clamp(0, 100) as i32,
        protected: join(c.permissions.protected.iter().map(|u| u.as_str())),
        audit_room: c.audit_room.as_ref().map(|r| r.as_str().into()).unwrap_or_default(),
        greet: c.greet,
        ..Default::default()
    }
    .with_card(c.card.as_ref())
}

/// Lines holding just this separate alternate greetings in the form.
const GREETING_BREAK: &str = "\n---\n";

impl AgentForm {
    /// The form showing `card` (or no card).
    pub fn with_card(mut self, card: Option<&Card>) -> Self {
        self.has_card = card.is_some();
        let Some(c) = card else { return self };
        self.card_name = c.name.as_str().into();
        self.card_nickname = c.nickname.as_str().into();
        self.card_description = c.description.as_str().into();
        self.card_personality = c.personality.as_str().into();
        self.card_scenario = c.scenario.as_str().into();
        self.card_first_mes = c.first_mes.as_str().into();
        self.card_greetings = c.alternate_greetings.join(GREETING_BREAK).into();
        self.card_examples = c.mes_example.as_str().into();
        self.card_system = c.system_prompt.as_str().into();
        self.card_post = c.post_history_instructions.as_str().into();
        self.card_tags = join(&c.tags);
        self.card_creator = c.creator.as_str().into();
        self.card_notes = c.creator_notes.as_str().into();
        let n = c.character_book.as_ref().map_or(0, |b| b.entries.len());
        self.card_lore = match n {
            0 => "".into(),
            1 => "1 lorebook entry".into(),
            n => format!("{n} lorebook entries").into(),
        };
        self
    }
}

/// `base` (the imported or saved card) with the form's edits, or `None`
/// when the form has no card.
pub fn card_from_form(f: &AgentForm, base: Option<&Card>) -> Result<Option<Card>> {
    if !f.has_card {
        return Ok(None);
    }
    let mut c = base.cloned().unwrap_or_default();
    c.name = f.card_name.trim().to_owned();
    if c.name.is_empty() {
        bail!("the character needs a name");
    }
    c.nickname = f.card_nickname.trim().to_owned();
    c.description = f.card_description.to_string();
    c.personality = f.card_personality.to_string();
    c.scenario = f.card_scenario.to_string();
    c.first_mes = f.card_first_mes.to_string();
    c.alternate_greetings = f.card_greetings.split(GREETING_BREAK).map(str::trim).filter(|g| !g.is_empty()).map(str::to_owned).collect();
    c.mes_example = f.card_examples.to_string();
    c.system_prompt = f.card_system.to_string();
    c.post_history_instructions = f.card_post.to_string();
    c.tags = list(&f.card_tags).map(str::to_owned).collect();
    c.creator = f.card_creator.trim().to_owned();
    c.creator_notes = f.card_notes.to_string();
    Ok(Some(c))
}

/// Builds a config from the form. `old` supplies the saved API key when the
/// key field is empty; `detected` is the result of the last "Connect".
/// `card` is the card to build on (just imported, or the saved one).
pub fn from_form(account: AccountId, f: &AgentForm, old: Option<&AgentConfig>, detected: Option<&Detected>, card: Option<&Card>) -> Result<AgentConfig> {
    let url = f.url.trim().to_owned();
    if url.is_empty() {
        bail!("enter the AI API's address");
    }
    let model = f.model.trim().to_owned();
    if model.is_empty() {
        bail!("pick or type a model name");
    }
    let new_key = f.api_key.trim().to_owned();
    let key = if new_key.is_empty() { old.and_then(|o| o.api.api_key.clone()) } else { Some(new_key) };

    // Keep what we know about the API unless the address changed.
    let mut api = match old {
        Some(o) if o.api.url == url => o.api.clone(),
        _ => ApiConfig { url: url.clone(), ..Default::default() },
    };
    api.url = url;
    api.api_key = key;
    api.model = model;
    if let Some(d) = detected {
        api.format = d.format;
        api.endpoint = d.endpoint.clone();
        api.service = d.service.clone();
    }

    let extra = f.extra_json.trim();
    let extra_params = if extra.is_empty() {
        Default::default()
    } else {
        match serde_json::from_str::<serde_json::Value>(extra).context("extra request settings must be JSON")? {
            serde_json::Value::Object(m) => m,
            _ => bail!("extra request settings must be a JSON object like {{\"temperature\": 0.7}}"),
        }
    };

    let mut cfg = match old {
        Some(o) => o.clone(),
        None => AgentConfig::new(account, api.clone()),
    };
    cfg.account = account;
    cfg.api = api;
    cfg.enabled = f.enabled;
    cfg.max_tokens = f.max_tokens.max(16) as u32;
    cfg.extra_params = extra_params;
    cfg.trigger = match f.trigger {
        1 => Trigger::Every,
        2 => Trigger::DirectOnly,
        _ => Trigger::Mention,
    };
    cfg.system_prompt = if f.system_prompt.trim().is_empty() { default_prompt() } else { f.system_prompt.to_string() };
    cfg.rooms = rooms(&f.rooms)?;
    cfg.history = f.history.max(2) as usize;
    cfg.summarize = f.summarize;
    cfg.summary_model = f.summary_model.trim().to_owned();
    cfg.context_tokens = f.context_tokens.max(500) as usize;
    cfg.yield_secs = f.yield_secs.max(1) as u64;
    cfg.max_rapid_replies = f.max_rapid.max(1) as u32;
    cfg.venice_system_prompt = f.venice_system_prompt;
    cfg.venice_web_search = f.venice_web_search;
    cfg.see_images = f.see_images;
    cfg.image_model = f.image_model.trim().to_owned();
    cfg.web = f.web;
    cfg.web_sites = f.web_sites.split([',', ' ', '\n']).map(|s| s.trim().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_ascii_lowercase()).filter(|s| !s.is_empty()).collect();
    cfg.reply_chars = f.reply_chars.max(0) as usize;
    cfg.cooldown_secs = f.cooldown.max(0) as u64;
    cfg.max_steps = f.max_steps.max(1) as usize;
    cfg.card = card_from_form(f, card)?;
    cfg.greet = f.greet;

    let p = &mut cfg.permissions;
    p.global = [
        (f.g_reply, Permission::Reply),
        (f.g_redact, Permission::Redact),
        (f.g_kick, Permission::Kick),
        (f.g_ban, Permission::Ban),
        (f.g_unban, Permission::Unban),
        (f.g_invite, Permission::Invite),
        (f.g_power, Permission::SetPowerLevel),
    ]
    .into_iter()
    .filter_map(|(on, perm)| on.then_some(perm))
    .collect();
    p.commanders.users = users(&f.commanders)?;
    p.commanders.min_power_level = f.min_level.into();
    p.autonomous = f.autonomous;
    p.max_grant_level = f.max_grant.into();
    p.protected = users(&f.protected)?;
    let audit = f.audit_room.trim();
    cfg.audit_room = if audit.is_empty() {
        None
    } else {
        Some(audit.try_into().with_context(|| format!("“{audit}” isn't a room id"))?)
    };
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keeps_saved_key() {
        let id = AccountId::new();
        let mut f = to_form(id, "bot", None);
        f.url = "openrouter.ai/api".into();
        f.api_key = "sk-test".into();
        f.model = "some/model".into();
        f.g_ban = true;
        f.extra_json = r#"{"temperature": 0.5}"#.into();
        f.commanders = "@me:anoncat.org".into();
        let cfg = from_form(id, &f, None, None, None).unwrap();
        assert!(cfg.permissions.global.contains(&Permission::Ban));
        assert_eq!(cfg.extra_params["temperature"], 0.5);

        // Saving again with an empty key field keeps the stored key.
        let mut f2 = to_form(id, "bot", Some(&cfg));
        assert!(f2.api_key_set);
        f2.g_ban = false;
        let cfg2 = from_form(id, &f2, Some(&cfg), None, None).unwrap();
        assert_eq!(cfg2.api.api_key.as_deref(), Some("sk-test"));
        assert!(!cfg2.permissions.global.contains(&Permission::Ban));
    }

    #[test]
    fn card_edits_keep_the_rest_of_the_card() {
        let id = AccountId::new();
        let base = Card::from_json(serde_json::json!({"spec": "chara_card_v3", "spec_version": "3.0", "data": {
            "name": "Mira", "alternate_greetings": ["a", "b"], "tags": ["x"],
            "extensions": {"keep": 1}, "character_book": {"entries": [{"keys": ["k"], "content": "c"}]}
        }})).unwrap();
        let mut f = to_form(id, "bot", None).with_card(Some(&base));
        assert_eq!(f.card_lore, "1 lorebook entry");
        assert_eq!(f.card_greetings, "a\n---\nb");
        f.url = "localhost:11434".into();
        f.model = "llama3".into();
        f.card_description = "edited".into();
        f.card_greetings = "a\n---\n\n---\nc".into();
        let cfg = from_form(id, &f, None, None, Some(&base)).unwrap();
        let c = cfg.card.as_ref().unwrap();
        assert_eq!(c.description, "edited");
        assert_eq!(c.alternate_greetings, ["a", "c"]);
        assert_eq!(c.character_book.as_ref().unwrap().entries.len(), 1);
        assert_eq!(c.to_v3()["data"]["extensions"]["keep"], 1);
        // Saved and shown again: the same card.
        assert_eq!(to_form(id, "bot", Some(&cfg)).card_description, "edited");
        f.card_name = " ".into();
        assert!(from_form(id, &f, None, None, Some(&base)).is_err());
        f.has_card = false;
        assert!(from_form(id, &f, None, None, Some(&base)).unwrap().card.is_none());
    }

    #[test]
    fn rejects_bad_input() {
        let id = AccountId::new();
        let mut f = to_form(id, "bot", None);
        assert!(from_form(id, &f, None, None, None).is_err(), "no url");
        f.url = "localhost:11434".into();
        assert!(from_form(id, &f, None, None, None).is_err(), "no model");
        f.model = "llama3".into();
        assert!(from_form(id, &f, None, None, None).is_ok(), "local servers need no key");
        f.commanders = "not a user".into();
        assert!(from_form(id, &f, None, None, None).is_err());
        f.commanders = "".into();
        f.extra_json = "[1]".into();
        assert!(from_form(id, &f, None, None, None).is_err());
    }
}
