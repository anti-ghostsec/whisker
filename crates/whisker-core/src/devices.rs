//! An account's sessions (devices) on its server: list, rename, remove.

use anyhow::{Result, bail};
use matrix_sdk::Client;
use matrix_sdk::ruma::OwnedDeviceId;

/// What Whisker calls its own sessions. A name many apps' sessions show,
/// so it doesn't tell anyone looking at the list (the server, or people
/// who can see your devices) which app you use, or tie accounts together.
pub const SESSION_NAME: &str = "Unnamed session";

/// One of the account's sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    /// Empty when it has none.
    pub name: String,
    /// When the server last saw it (ms since 1970).
    pub last_seen: Option<u64>,
    /// The session Whisker is signed in with here.
    pub current: bool,
}

/// The account's sessions: this one first, then the most recently seen.
pub async fn list(client: &Client) -> Result<Vec<Session>> {
    let me = client.device_id().map(|d| d.to_owned());
    let mut out: Vec<Session> = client
        .devices()
        .await?
        .devices
        .into_iter()
        .map(|d| Session {
            current: me.as_ref() == Some(&d.device_id),
            id: d.device_id.to_string(),
            name: d.display_name.unwrap_or_default(),
            last_seen: d.last_seen_ts.map(|t| u64::from(t.get())),
        })
        .collect();
    out.sort_by(|a, b| b.current.cmp(&a.current).then(b.last_seen.cmp(&a.last_seen)));
    Ok(out)
}

/// Renames one of the account's sessions (this one too).
pub async fn rename(client: &Client, id: &str, name: &str) -> Result<()> {
    let id = OwnedDeviceId::from(id);
    client.rename_device(&id, name.trim()).await?;
    Ok(())
}

/// Signs one of the account's other sessions out; the server wants the
/// account password for it. This session is signed out with Log out.
pub async fn remove(client: &Client, id: &str, password: &str) -> Result<()> {
    if client.device_id().is_some_and(|d| d.as_str() == id) {
        bail!("that's this session: use Log out for it");
    }
    let ids = [OwnedDeviceId::from(id)];
    let Err(e) = client.delete_devices(&ids, None).await else { return Ok(()) };
    let Some(info) = e.as_uiaa_response() else { return Err(e.into()) };
    let auth = crate::verify::password_auth(client, password, info.session.clone())?;
    client.delete_devices(&ids, Some(auth)).await?;
    Ok(())
}
