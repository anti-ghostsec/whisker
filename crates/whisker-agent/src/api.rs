//! Connecting to whatever model API the user points us at.
//!
//! The user gives a URL (and maybe a key). [`detect`] works out which kind
//! of API lives there and which models it offers, so any OpenAI-style
//! server (OpenRouter, Groq, Mistral, DeepSeek, Gemini's OpenAI endpoint,
//! LM Studio, llama.cpp, vLLM, Ollama …) or Anthropic-style server works
//! without the user knowing the difference.

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiFormat {
    /// Not detected yet; detection runs before the first request.
    #[default]
    Unknown,
    /// `POST …/chat/completions`
    OpenAiChat,
    /// `POST …/messages`
    AnthropicMessages,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ApiConfig {
    /// What the user typed, e.g. `https://openrouter.ai/api` or `localhost:11434`.
    pub url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub format: ApiFormat,
    /// Full request URL, filled in by detection.
    #[serde(default)]
    pub endpoint: String,
    /// Human name of the service, filled in by detection ("OpenRouter").
    #[serde(default)]
    pub service: String,
}

impl std::fmt::Debug for ApiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}) {}", self.service, self.url, self.model)
    }
}

#[derive(Debug, Clone)]
pub struct Detected {
    pub format: ApiFormat,
    pub endpoint: String,
    pub service: String,
    pub models: Vec<String>,
    /// The ones among `models` that can see pictures (by what the service
    /// says, or by name).
    pub vision: Vec<String>,
    /// The service's own pick for chatting (Venice marks one), else a
    /// widely liked model it offers.
    pub default_model: Option<String>,
    /// Likewise for looking at pictures.
    pub default_vision: Option<String>,
}

/// A service's model list.
#[derive(Debug, Default)]
struct Listed {
    models: Vec<String>,
    vision: Vec<String>,
    default_model: Option<String>,
    default_vision: Option<String>,
}

impl Detected {
    fn new(format: ApiFormat, endpoint: String, service: String, l: Listed) -> Self {
        let default_model = l.default_model.or_else(|| popular(&l.models, CHAT_PICKS));
        let default_vision = l.default_vision.or_else(|| popular(&l.vision, VISION_PICKS));
        Detected { format, endpoint, service, models: l.models, vision: l.vision, default_model, default_vision }
    }
}

/// Well-regarded, quick models, best first, matched by name for services
/// that don't mark a default.
const CHAT_PICKS: &[&str] = &[
    "claude-sonnet-4", "gpt-4.1-mini", "gpt-4o-mini", "gemini-2.5-flash", "gemini-2.0-flash", "deepseek-chat", "deepseek-v3",
    "grok-3-mini", "mistral-small", "llama-3.3-70b", "qwen3", "llama3", "mistral",
];
const VISION_PICKS: &[&str] = &[
    "claude-sonnet-4", "gpt-4.1-mini", "gpt-4o-mini", "gemini-2.5-flash", "gemini-2.0-flash", "qwen2.5-vl", "pixtral", "llama-4",
    "gemma-3", "gemma3", "llava", "minicpm-v", "moondream",
];

/// The first of `picks` that one of `models` is named like.
fn popular(models: &[String], picks: &[&str]) -> Option<String> {
    picks.iter().find_map(|p| models.iter().find(|m| m.to_lowercase().contains(p)).cloned())
}

/// The model a Venice-style list marks with `trait_name` in `model_spec.traits`.
fn marked(v: &Value, trait_name: &str) -> Option<String> {
    v.get("data")?.as_array()?.iter().find_map(|m| {
        let traits = m.pointer("/model_spec/traits")?.as_array()?;
        traits.iter().any(|t| t == trait_name).then(|| m.get("id")?.as_str().map(str::to_owned))?
    })
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().timeout(Duration::from_secs(15)).build().expect("http client")
}

/// Adds a scheme if missing (http for local addresses, https otherwise) and
/// trims trailing slashes.
pub fn normalize_url(input: &str) -> String {
    let t = input.trim().trim_end_matches('/');
    if t.contains("://") {
        return t.to_owned();
    }
    let local = t.starts_with("localhost") || t.starts_with("127.") || t.starts_with("0.0.0.0") || t.starts_with("[::1]")
        || t.starts_with("192.168.") || t.starts_with("10.") || t.ends_with(".local") || t.split(':').next().is_some_and(|h| h.ends_with(".lan"));
    format!("{}://{t}", if local { "http" } else { "https" })
}

fn service_name(url: &reqwest::Url) -> String {
    let host = url.host_str().unwrap_or_default();
    let known = [
        ("openrouter.ai", "OpenRouter"),
        ("api.openai.com", "OpenAI"),
        ("api.groq.com", "Groq"),
        ("api.mistral.ai", "Mistral"),
        ("api.deepseek.com", "DeepSeek"),
        ("generativelanguage.googleapis.com", "Google Gemini"),
        ("api.x.ai", "xAI"),
        ("api.together.xyz", "Together"),
        ("api.anthropic.com", "Anthropic"),
        ("api.perplexity.ai", "Perplexity"),
        ("api.venice.ai", "Venice"),
    ];
    if let Some((_, n)) = known.iter().find(|(h, _)| host.ends_with(h)) {
        return (*n).to_owned();
    }
    if matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") || host.starts_with("192.168.") || host.starts_with("10.") {
        return "Local server".into();
    }
    host.to_owned()
}

/// Whether a model's name says it can see pictures (for services whose
/// model list doesn't).
pub fn looks_vision(name: &str) -> bool {
    let n = name.to_lowercase();
    ["vision", "-vl", "_vl", "vl-", "llava", "pixtral", "gpt-4o", "gpt-4.1", "gpt-5", "claude", "gemini", "gemma-3", "gemma3",
        "mistral-small-3", "mistral-31", "mistral-medium", "llama-4", "llama4", "minicpm-v", "moondream", "qwen2.5-vl", "qwen-vl", "grok-2-vision", "grok-4"]
        .iter()
        .any(|k| n.contains(k))
}

/// The models in a model list that can see pictures: what the service
/// says (Venice, OpenRouter), else a guess from the name.
fn vision(v: &Value, list: &str, field: &str) -> Vec<String> {
    let Some(arr) = v.get(list).and_then(Value::as_array) else { return Vec::new() };
    let mut out: Vec<String> = arr
        .iter()
        .filter_map(|m| {
            let id = m.get(field)?.as_str()?;
            let said = m.pointer("/model_spec/capabilities/supportsVision").and_then(Value::as_bool).or_else(|| {
                let inputs = m.pointer("/architecture/input_modalities")?.as_array()?;
                Some(inputs.iter().any(|i| i == "image"))
            });
            said.unwrap_or_else(|| looks_vision(id)).then(|| id.to_owned())
        })
        .collect();
    out.sort();
    out
}

fn ids(v: &Value, list: &str, field: &str) -> Option<Vec<String>> {
    let arr = v.get(list)?.as_array()?;
    let mut out: Vec<String> = arr.iter().filter_map(|m| m.get(field)?.as_str().map(str::to_owned)).collect();
    out.sort();
    Some(out)
}

async fn get_json(req: reqwest::RequestBuilder) -> Result<(u16, Option<Value>)> {
    let resp = req.send().await?;
    let status = resp.status().as_u16();
    Ok((status, resp.json::<Value>().await.ok()))
}

/// Finds out what API is at `url`. Tries the most specific guess first.
pub async fn detect(url: &str, key: Option<&str>) -> Result<Detected> {
    let full = normalize_url(url);
    let parsed = reqwest::Url::parse(&full).map_err(|_| anyhow!("that isn't a valid address"))?;
    let service = service_name(&parsed);
    let origin = parsed.origin().ascii_serialization();
    let key = key.filter(|k| !k.trim().is_empty()).map(str::trim);
    let c = http();

    // A full endpoint URL was pasted: trust its shape.
    for (suffix, format) in [("/chat/completions", ApiFormat::OpenAiChat), ("/messages", ApiFormat::AnthropicMessages)] {
        if let Some(base) = full.strip_suffix(suffix) {
            let listed = list_models(&c, base, format, key).await.unwrap_or_default();
            return Ok(Detected::new(format, full.clone(), service, listed));
        }
    }

    let mut bases = vec![full.clone()];
    if service == "Google Gemini" && !full.contains("/openai") {
        bases.insert(0, format!("{origin}/v1beta/openai"));
    }
    if !full.ends_with("/v1") && !full.contains("/openai") {
        bases.push(format!("{full}/v1"));
    }
    let anthropic_first = service == "Anthropic";
    let order = if anthropic_first {
        [ApiFormat::AnthropicMessages, ApiFormat::OpenAiChat]
    } else {
        [ApiFormat::OpenAiChat, ApiFormat::AnthropicMessages]
    };

    let mut rejected = false;
    for format in order {
        for base in &bases {
            match list_models(&c, base, format, key).await {
                Ok(listed) => {
                    let path = if format == ApiFormat::OpenAiChat { "chat/completions" } else { "messages" };
                    return Ok(Detected::new(format, format!("{base}/{path}"), service, listed));
                }
                Err(e) if e.to_string() == "unauthorized" => rejected = true,
                Err(_) => {}
            }
        }
    }

    // Ollama's own API (it also speaks OpenAI at /v1).
    if let Ok((200, Some(v))) = get_json(c.get(format!("{origin}/api/tags"))).await
        && let Some(models) = ids(&v, "models", "name")
    {
        let vision = models.iter().filter(|m| looks_vision(m)).cloned().collect();
        let listed = Listed { models, vision, ..Default::default() };
        return Ok(Detected::new(ApiFormat::OpenAiChat, format!("{origin}/v1/chat/completions"), "Ollama".into(), listed));
    }

    if rejected {
        bail!("the server is there but rejected the API key");
    }
    bail!("couldn't find an AI API at that address")
}

/// The service's models, the ones among them that can see pictures, and
/// any it marks as its defaults.
async fn list_models(c: &reqwest::Client, base: &str, format: ApiFormat, key: Option<&str>) -> Result<Listed> {
    let mut req = c.get(format!("{base}/models"));
    match format {
        ApiFormat::AnthropicMessages => {
            req = req.header("anthropic-version", "2023-06-01");
            if let Some(k) = key {
                req = req.header("x-api-key", k);
            }
        }
        _ => {
            if let Some(k) = key {
                req = req.bearer_auth(k);
            }
        }
    }
    let (status, body) = get_json(req).await?;
    if status == 401 || status == 403 {
        bail!("unauthorized");
    }
    if status != 200 {
        bail!("status {status}");
    }
    let v = body.ok_or_else(|| anyhow!("not JSON"))?;
    // Anthropic model objects carry "type": "model"; OpenAI ones "object": "model".
    let looks_anthropic = v["data"].as_array().and_then(|a| a.first()).is_some_and(|m| m["type"] == "model");
    if format == ApiFormat::AnthropicMessages && !looks_anthropic && v["data"].as_array().is_some_and(|a| !a.is_empty()) {
        bail!("not an Anthropic-style API");
    }
    let models = ids(&v, "data", "id").ok_or_else(|| anyhow!("no model list"))?;
    Ok(Listed { models, vision: vision(&v, "data", "id"), default_model: marked(&v, "default"), default_vision: marked(&v, "default_vision") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_defaults() {
        let v = serde_json::json!({"data": [
            {"id": "big", "model_spec": {"traits": ["most_intelligent"]}},
            {"id": "quick", "model_spec": {"traits": ["default", "fastest"]}},
            {"id": "eyes", "model_spec": {"traits": ["default_vision"]}},
        ]});
        assert_eq!(marked(&v, "default").as_deref(), Some("quick"));
        assert_eq!(marked(&v, "default_vision").as_deref(), Some("eyes"));
        assert_eq!(marked(&v, "default_code"), None);
        let models = ["llama3.2:3b".to_owned(), "openai/gpt-4o-mini".to_owned()];
        assert_eq!(popular(&models, CHAT_PICKS).as_deref(), Some("openai/gpt-4o-mini"));
    }

    #[test]
    fn finds_vision_models() {
        let v = serde_json::json!({"data": [
            {"id": "venice-uncensored", "model_spec": {"capabilities": {"supportsVision": false}}},
            {"id": "mistral-31-24b", "model_spec": {"capabilities": {"supportsVision": true}}},
            {"id": "a/text", "architecture": {"input_modalities": ["text"]}},
            {"id": "a/sees", "architecture": {"input_modalities": ["text", "image"]}},
            {"id": "llava:13b"},
            {"id": "llama3:8b"},
        ]});
        assert_eq!(vision(&v, "data", "id"), ["a/sees", "llava:13b", "mistral-31-24b"]);
    }

    #[test]
    fn normalizes() {
        assert_eq!(normalize_url("localhost:11434/"), "http://localhost:11434");
        assert_eq!(normalize_url("openrouter.ai/api"), "https://openrouter.ai/api");
        assert_eq!(normalize_url(" https://x.org/v1 "), "https://x.org/v1");
        assert_eq!(normalize_url("192.168.1.5:8080"), "http://192.168.1.5:8080");
    }

    #[test]
    fn names_services() {
        let u = |s: &str| reqwest::Url::parse(s).unwrap();
        assert_eq!(service_name(&u("https://openrouter.ai/api/v1")), "OpenRouter");
        assert_eq!(service_name(&u("https://api.venice.ai/api/v1")), "Venice");
        assert_eq!(service_name(&u("http://127.0.0.1:8080")), "Local server");
        assert_eq!(service_name(&u("https://ai.example.com")), "ai.example.com");
    }
}
