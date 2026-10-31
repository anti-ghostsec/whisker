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
    ];
    if let Some((_, n)) = known.iter().find(|(h, _)| host.ends_with(h)) {
        return (*n).to_owned();
    }
    if matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") || host.starts_with("192.168.") || host.starts_with("10.") {
        return "Local server".into();
    }
    host.to_owned()
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
            let models = list_models(&c, base, format, key).await.unwrap_or_default();
            return Ok(Detected { format, endpoint: full.clone(), service, models });
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
                Ok(models) => {
                    let path = if format == ApiFormat::OpenAiChat { "chat/completions" } else { "messages" };
                    return Ok(Detected { format, endpoint: format!("{base}/{path}"), service, models });
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
        return Ok(Detected {
            format: ApiFormat::OpenAiChat,
            endpoint: format!("{origin}/v1/chat/completions"),
            service: "Ollama".into(),
            models,
        });
    }

    if rejected {
        bail!("the server is there but rejected the API key");
    }
    bail!("couldn't find an AI API at that address")
}

async fn list_models(c: &reqwest::Client, base: &str, format: ApiFormat, key: Option<&str>) -> Result<Vec<String>> {
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
    ids(&v, "data", "id").ok_or_else(|| anyhow!("no model list"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(service_name(&u("http://127.0.0.1:8080")), "Local server");
        assert_eq!(service_name(&u("https://ai.example.com")), "ai.example.com");
    }
}
