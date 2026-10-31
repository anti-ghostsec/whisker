//! Server extensions: features a homeserver adds through hot-loaded WASM
//! plugins (anoncat), discovered at runtime instead of compiled in.
//!
//! See `docs/PLUGIN_PROTOCOL.md` for the wire format a server implements.
//! Servers without the extension API (Synapse, Conduit, ...) just report no
//! plugins, and every feature that needs one stays hidden.

use anyhow::{Result, bail};
use matrix_sdk::Client;
use serde::{Deserialize, Serialize};

const BASE: &str = "_anoncat/client/v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInfo {
    /// Reverse-DNS id, e.g. `org.anoncat.ephemeral`.
    pub id: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// Feature flags the client keys UI off, e.g. `ephemeral_rooms`.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Methods callable through [`ServerExtensions::call`].
    #[serde(default)]
    pub methods: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerExtensions {
    #[serde(default)]
    pub plugins: Vec<PluginInfo>,
    /// Bumped by the server whenever a plugin is loaded, reloaded or removed.
    #[serde(default)]
    pub generation: u64,
}

impl ServerExtensions {
    /// Asks the homeserver which plugins it's running. A server without
    /// the extension API yields an empty set, not an error.
    pub async fn discover(client: &Client) -> Result<Self> {
        let url = format!("{}{BASE}/plugins", base(client));
        let mut req = client.http_client().get(url);
        if let Some(t) = client.access_token() {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await?;
        if matches!(resp.status().as_u16(), 400 | 404 | 405 | 501) {
            return Ok(Self::default());
        }
        Ok(resp.error_for_status()?.json().await?)
    }

    pub fn has(&self, capability: &str) -> bool {
        self.plugins.iter().any(|p| p.capabilities.iter().any(|c| c == capability))
    }

    pub fn plugin(&self, id: &str) -> Option<&PluginInfo> {
        self.plugins.iter().find(|p| p.id == id)
    }

    /// Calls `method` on plugin `id` with a JSON body.
    pub async fn call(
        &self,
        client: &Client,
        id: &str,
        method: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let Some(p) = self.plugin(id) else { bail!("server has no plugin {id}") };
        if !p.methods.iter().any(|m| m == method) {
            bail!("plugin {id} has no method {method}");
        }
        let url = format!("{}{BASE}/plugins/{}/call/{}", base(client), enc(id), enc(method));
        let mut req = client.http_client().post(url).json(body);
        if let Some(t) = client.access_token() {
            req = req.bearer_auth(t);
        }
        Ok(req.send().await?.error_for_status()?.json().await?)
    }
}

fn base(client: &Client) -> String {
    let mut s = client.homeserver().to_string();
    if !s.ends_with('/') {
        s.push('/');
    }
    s
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'-' | b'_' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_listing() {
        let j = r#"{"generation":3,"plugins":[{"id":"org.anoncat.ephemeral","version":"0.1.0",
            "capabilities":["ephemeral_rooms"],"methods":["create"]}]}"#;
        let ext: ServerExtensions = serde_json::from_str(j).unwrap();
        assert!(ext.has("ephemeral_rooms"));
        assert!(!ext.has("nope"));
        assert_eq!(ext.generation, 3);
        assert_eq!(enc("a/b c"), "a%2Fb%20c");
    }
}
