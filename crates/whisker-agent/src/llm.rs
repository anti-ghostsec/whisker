//! Talks to the model API. The conversation is kept in Anthropic Messages
//! format (a `messages` array of JSON values) and converted when the API
//! speaks OpenAI chat completions.

use crate::api::{self, ApiConfig, ApiFormat};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::time::Duration;
use tokio::sync::OnceCell;

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug)]
pub struct Turn {
    /// The assistant message to append to the conversation, unchanged
    /// (it may carry blocks the API wants back as-is).
    pub assistant: Value,
    pub text: String,
    pub calls: Vec<ToolCall>,
    /// The model declined; don't post anything.
    pub refused: bool,
}

/// Venice-only switches. Both off unless the owner turns them on: Venice's
/// own system prompt would sit beside ours, and web search would send chat
/// messages on to a third-party search engine.
#[derive(Debug, Clone, Copy, Default)]
pub struct Venice {
    pub system_prompt: bool,
    pub web_search: bool,
}

pub struct Llm {
    api: ApiConfig,
    max_tokens: u32,
    extra: Map<String, Value>,
    venice: Venice,
    http: reqwest::Client,
    /// Format and endpoint, detected on first use if the config has none.
    resolved: OnceCell<(ApiFormat, String)>,
}

impl Llm {
    pub fn new(api: ApiConfig, max_tokens: u32, extra: Map<String, Value>) -> Result<Self> {
        let http = whisker_core::tls::http_builder().timeout(Duration::from_secs(300)).build()?;
        Ok(Self { api, max_tokens, extra, http, resolved: OnceCell::new(), venice: Venice::default() })
    }

    pub fn with_venice(mut self, venice: Venice) -> Self {
        self.venice = venice;
        self
    }

    fn is_venice(&self, endpoint: &str) -> bool {
        self.api.service == "Venice"
            || reqwest::Url::parse(endpoint).ok().and_then(|u| u.host_str().map(|h| h == "venice.ai" || h.ends_with(".venice.ai"))).unwrap_or(false)
    }

    async fn resolve(&self) -> Result<&(ApiFormat, String)> {
        self.resolved
            .get_or_try_init(|| async {
                if self.api.format != ApiFormat::Unknown && !self.api.endpoint.is_empty() {
                    return Ok((self.api.format, self.api.endpoint.clone()));
                }
                let d = api::detect(&self.api.url, self.api.api_key.as_deref()).await?;
                Ok::<_, anyhow::Error>((d.format, d.endpoint))
            })
            .await
    }

    /// One model call. `system` comes in parts (the app's contract first,
    /// then the agent's persona), sent as separate system blocks where the
    /// API has them, so a persona can't run into or bury the contract.
    pub async fn turn(&self, system: &[&str], messages: &[Value], tools: &[ToolDef]) -> Result<Turn> {
        if self.api.model.trim().is_empty() {
            bail!("no model chosen for this AI");
        }
        let (format, endpoint) = self.resolve().await?;
        match format {
            ApiFormat::AnthropicMessages => self.anthropic(endpoint, system, messages, tools).await,
            _ => self.openai(endpoint, system, messages, tools).await,
        }
    }

    /// Sends one tiny request and returns the reply, to check the settings work.
    pub async fn test(&self) -> Result<String> {
        let t = self
            .turn(&["You are being tested. Reply with one short friendly sentence."], &[json!({"role": "user", "content": "Hello!"})], &[])
            .await?;
        if t.refused {
            bail!("the model declined to answer");
        }
        Ok(t.text)
    }

    fn merge_extra(&self, body: &mut Value) {
        if let Some(obj) = body.as_object_mut() {
            for (k, v) in &self.extra {
                obj.insert(k.clone(), v.clone());
            }
        }
    }

    async fn anthropic(&self, endpoint: &str, system: &[&str], messages: &[Value], tools: &[ToolDef]) -> Result<Turn> {
        let system: Vec<Value> = system.iter().filter(|s| !s.trim().is_empty()).map(|s| json!({ "type": "text", "text": s })).collect();
        let mut body = json!({
            "model": self.api.model,
            "max_tokens": self.max_tokens,
            "system": system,
            "messages": messages,
        });
        if !tools.is_empty() {
            body["tools"] = tools
                .iter()
                .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.schema }))
                .collect();
        }
        self.merge_extra(&mut body);
        let version = if endpoint.contains("api.anthropic.com") { "2026-10-21" } else { "2023-06-01" };
        let key = self.api.api_key.clone().unwrap_or_default();
        let resp: Value = send_with_retry(|b: &Value| {
            self.http.post(endpoint).header("x-api-key", &key).header("anthropic-version", version).json(b)
        }, body)
        .await?;

        let stop = resp["stop_reason"].as_str().unwrap_or_default();
        let content = resp["content"].as_array().cloned().unwrap_or_default();
        let text = content.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("");
        // A response cut off at max_tokens may hold a truncated tool input; never run it.
        let calls = if stop == "tool_use" {
            content
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .map(|b| ToolCall {
                    id: b["id"].as_str().unwrap_or_default().to_owned(),
                    name: b["name"].as_str().unwrap_or_default().to_owned(),
                    input: b["input"].clone(),
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(Turn { assistant: json!({ "role": "assistant", "content": content }), text, calls, refused: stop == "refusal" })
    }

    async fn openai(&self, endpoint: &str, system: &[&str], messages: &[Value], tools: &[ToolDef]) -> Result<Turn> {
        // Many OpenAI-style servers accept only one system message.
        let system = system.iter().filter(|s| !s.trim().is_empty()).copied().collect::<Vec<_>>().join("\n\n");
        let mut msgs = vec![json!({ "role": "system", "content": system })];
        msgs.extend(to_openai(messages));
        let mut body = json!({ "model": self.api.model, "messages": msgs, "max_tokens": self.max_tokens });
        if !tools.is_empty() {
            body["tools"] = tools
                .iter()
                .map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.schema } }))
                .collect();
        }
        self.merge_extra(&mut body);
        if self.is_venice(endpoint) {
            // Safe defaults first; anything set in the advanced JSON still wins.
            let mut vp = serde_json::Map::new();
            vp.insert("include_venice_system_prompt".into(), json!(self.venice.system_prompt));
            vp.insert("enable_web_search".into(), json!(if self.venice.web_search { "auto" } else { "off" }));
            if let Some(Value::Object(own)) = body.get("venice_parameters") {
                vp.extend(own.clone());
            }
            body["venice_parameters"] = Value::Object(vp);
        }
        let key = self.api.api_key.clone().filter(|k| !k.is_empty());
        let resp: Value = send_with_retry(|b: &Value| {
            let mut req = self.http.post(endpoint).json(b);
            if let Some(k) = &key {
                req = req.bearer_auth(k);
            }
            req
        }, body)
        .await?;

        let choice = &resp["choices"][0];
        let msg = &choice["message"];
        let text = msg["content"].as_str().unwrap_or_default().to_owned();
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(json!({ "type": "text", "text": text }));
        }
        let mut calls = Vec::new();
        if choice["finish_reason"] != "length" {
            for c in msg["tool_calls"].as_array().into_iter().flatten() {
                let input: Value = c["function"]["arguments"].as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
                let call = ToolCall {
                    id: c["id"].as_str().unwrap_or_default().to_owned(),
                    name: c["function"]["name"].as_str().unwrap_or_default().to_owned(),
                    input,
                };
                content.push(json!({ "type": "tool_use", "id": call.id, "name": call.name, "input": call.input }));
                calls.push(call);
            }
        }
        Ok(Turn { assistant: json!({ "role": "assistant", "content": content }), text, calls, refused: false })
    }
}

/// The user message carrying the results of one assistant turn's tool calls.
/// All results go back together in a single message.
pub fn tool_results(results: Vec<(String, Result<String>)>) -> Value {
    let content: Vec<Value> = results
        .into_iter()
        .map(|(id, r)| match r {
            Ok(s) => json!({ "type": "tool_result", "tool_use_id": id, "content": s }),
            Err(e) => json!({ "type": "tool_result", "tool_use_id": id, "content": format!("{e:#}"), "is_error": true }),
        })
        .collect();
    json!({ "role": "user", "content": content })
}

/// Anthropic-format messages to OpenAI chat format.
fn to_openai(messages: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in messages {
        let role = m["role"].as_str().unwrap_or("user");
        match &m["content"] {
            Value::String(s) => out.push(json!({ "role": role, "content": s })),
            Value::Array(blocks) if role == "assistant" => {
                let text: String = blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
                let calls: Vec<Value> = blocks
                    .iter()
                    .filter(|b| b["type"] == "tool_use")
                    .map(|b| json!({ "id": b["id"], "type": "function", "function": { "name": b["name"], "arguments": b["input"].to_string() } }))
                    .collect();
                let mut msg = json!({ "role": "assistant", "content": text });
                if !calls.is_empty() {
                    msg["tool_calls"] = Value::Array(calls);
                }
                out.push(msg);
            }
            Value::Array(blocks) => {
                // Text and pictures stay one user message: some servers
                // want user and assistant turns to alternate.
                let mut parts = Vec::new();
                for b in blocks {
                    match b["type"].as_str() {
                        Some("tool_result") => out.push(json!({ "role": "tool", "tool_call_id": b["tool_use_id"], "content": b["content"] })),
                        Some("text") => parts.push(json!({ "type": "text", "text": b["text"] })),
                        Some("image") => {
                            let src = &b["source"];
                            let url = format!("data:{};base64,{}", src["media_type"].as_str().unwrap_or("image/jpeg"), src["data"].as_str().unwrap_or_default());
                            parts.push(json!({ "type": "image_url", "image_url": { "url": url } }));
                        }
                        _ => {}
                    }
                }
                if parts.iter().all(|p| p["type"] == "text") {
                    let text: Vec<&str> = parts.iter().filter_map(|p| p["text"].as_str()).collect();
                    if !text.is_empty() {
                        out.push(json!({ "role": "user", "content": text.join("\n\n") }));
                    }
                } else {
                    out.push(json!({ "role": "user", "content": parts }));
                }
            }
            _ => {}
        }
    }
    out
}

/// Sends with retries on rate limits and server errors, and adapts to a
/// couple of common API differences (`max_tokens` vs
/// `max_completion_tokens`, servers that reject tools).
async fn send_with_retry(build: impl Fn(&Value) -> reqwest::RequestBuilder, mut body: Value) -> Result<Value> {
    let mut attempt = 0;
    let mut adapted = false;
    loop {
        let resp = build(&body).send().await.context("the AI API is unreachable")?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp.json().await?);
        }
        let retryable = status.as_u16() == 429 || status.is_server_error();
        if retryable && attempt < 2 {
            let wait = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(2u64.pow(attempt + 1));
            tokio::time::sleep(Duration::from_secs(wait.min(30))).await;
            attempt += 1;
            continue;
        }
        let text = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().or(v["error"].as_str()).map(str::to_owned))
            .unwrap_or(text);
        if status.as_u16() == 400 && !adapted && msg.contains("max_tokens") && body.get("max_tokens").is_some() {
            // Newer OpenAI models want max_completion_tokens instead.
            adapted = true;
            if let Some(o) = body.as_object_mut() {
                let v = o.remove("max_tokens").unwrap();
                o.insert("max_completion_tokens".into(), v);
            }
            continue;
        }
        return Err(match status.as_u16() {
            401 | 403 => anyhow!("the AI API rejected the key"),
            404 => anyhow!("the AI API says not found (wrong model name or URL?): {msg}"),
            _ => anyhow!("AI API error {status}: {msg}"),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_tool_round_trip_to_openai() {
        let msgs = vec![
            json!({ "role": "user", "content": "ban the spammer" }),
            json!({ "role": "assistant", "content": [
                { "type": "text", "text": "On it." },
                { "type": "tool_use", "id": "t1", "name": "ban", "input": { "user": "@s:x.org" } }
            ]}),
            tool_results(vec![("t1".into(), Ok("done".into()))]),
        ];
        let o = to_openai(&msgs);
        assert_eq!(o[1]["tool_calls"][0]["function"]["name"], "ban");
        assert_eq!(o[2]["role"], "tool");
        assert_eq!(o[2]["tool_call_id"], "t1");
    }

    #[test]
    fn pictures_go_to_openai_as_data_urls() {
        let msgs = vec![json!({ "role": "user", "content": [
            { "type": "text", "text": "look" },
            { "type": "image", "source": { "type": "base64", "media_type": "image/jpeg", "data": "QUJD" } },
            { "type": "text", "text": "end" }
        ]})];
        let o = to_openai(&msgs);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0]["content"][1]["image_url"]["url"], "data:image/jpeg;base64,QUJD");
        let text_only = to_openai(&[json!({ "role": "user", "content": [{ "type": "text", "text": "a" }, { "type": "text", "text": "b" }] })]);
        assert_eq!(text_only[0]["content"], "a\n\nb");
    }
}
