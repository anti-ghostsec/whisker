//! Per-agent settings. Stored inside the vault (they hold API keys).

use crate::api::{ApiConfig, ApiFormat};
use crate::permissions::Permissions;
use matrix_sdk::ruma::OwnedRoomId;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeSet;
use whisker_core::AccountId;

/// Key under which agents live in a vault's extras.
pub const VAULT_KEY: &str = "agents";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// The automated account this agent speaks as (an `AccountKind::Agent`).
    pub account: AccountId,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(alias = "provider", deserialize_with = "api_compat")]
    pub api: ApiConfig,
    /// Most tokens per reply.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Extra JSON fields merged into every request (temperature,
    /// reasoning_effort, top_p, …), for whatever the API supports.
    #[serde(default)]
    pub extra_params: serde_json::Map<String, serde_json::Value>,
    #[serde(default = "default_prompt")]
    pub system_prompt: String,
    #[serde(default)]
    pub trigger: Trigger,
    /// Rooms the agent works in. Empty means every room it's joined.
    #[serde(default)]
    pub rooms: BTreeSet<OwnedRoomId>,
    #[serde(default)]
    pub permissions: Permissions,
    /// Recent messages per room kept (in memory only) as context.
    #[serde(default = "default_history")]
    pub history: usize,
    /// Tool-use round trips allowed per message before giving up.
    #[serde(default = "default_steps")]
    pub max_steps: usize,
    /// Minimum seconds between replies in one room.
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
    /// Room where every moderation action is logged. Recommended.
    #[serde(default)]
    pub audit_room: Option<OwnedRoomId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// Reply when mentioned by name or MXID, and to every DM.
    #[default]
    Mention,
    /// Read and consider every message (needed for autonomous moderation).
    Every,
    /// Only in direct messages.
    DirectOnly,
}

fn yes() -> bool {
    true
}
fn default_history() -> usize {
    30
}
fn default_steps() -> usize {
    6
}
fn default_cooldown() -> u64 {
    2
}
fn default_max_tokens() -> u32 {
    1024
}
pub fn default_prompt() -> String {
    "You are a helpful assistant in a Matrix chat room. Keep replies short and conversational.".into()
}

impl AgentConfig {
    pub fn new(account: AccountId, api: ApiConfig) -> Self {
        let mut c: Self = serde_json::from_value(serde_json::json!({ "account": account, "api": {"url": ""} }))
            .expect("defaults are valid");
        c.api = api;
        c
    }
}

/// Reads both the current `api` shape and the first version's `provider` enum.
fn api_compat<'de, D: Deserializer<'de>>(d: D) -> Result<ApiConfig, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Compat {
        Tagged(Legacy),
        Current(ApiConfig),
    }
    #[derive(Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum Legacy {
        Anthropic { api_key: String, model: String, #[serde(default)] base_url: Option<String> },
        OpenAiCompatible { base_url: String, api_key: Option<String>, model: String },
    }
    Ok(match Compat::deserialize(d)? {
        Compat::Current(c) => c,
        Compat::Tagged(Legacy::Anthropic { api_key, model, base_url }) => {
            let base = base_url.unwrap_or_else(|| "https://api.anthropic.com".into());
            ApiConfig {
                endpoint: format!("{}/v1/messages", base.trim_end_matches('/')),
                url: base,
                api_key: Some(api_key),
                model,
                format: ApiFormat::AnthropicMessages,
                service: "Anthropic".into(),
            }
        }
        Compat::Tagged(Legacy::OpenAiCompatible { base_url, api_key, model }) => ApiConfig {
            url: base_url,
            api_key,
            model,
            ..Default::default()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_legacy_provider() {
        let id = AccountId::new();
        let j = serde_json::json!({
            "account": id,
            "provider": { "kind": "open_ai_compatible", "base_url": "http://127.0.0.1:8080", "model": "llama" }
        });
        let c: AgentConfig = serde_json::from_value(j).unwrap();
        assert_eq!(c.api.url, "http://127.0.0.1:8080");
        assert_eq!(c.api.format, ApiFormat::Unknown);
        assert_eq!(c.max_tokens, 1024);
    }
}
