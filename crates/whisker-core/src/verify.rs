//! Interactive verification (the emoji comparison), started by us or by
//! the other side. Progress is reported as [`ClientEvent::Verification`].

use crate::events::ClientEvent;
use crate::profile::AccountId;
use anyhow::{Result, anyhow, bail};
use futures_util::StreamExt;
use matrix_sdk::Client;
use matrix_sdk::encryption::verification::{SasState, SasVerification, VerificationRequest, VerificationRequestState};
use matrix_sdk::ruma::{OwnedUserId, UserId};
use tokio::sync::broadcast;

/// Where a verification stands, for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyState {
    /// Someone asked to verify with us; accept or decline.
    Incoming { self_verification: bool },
    /// We asked; waiting for them to accept on one of their devices.
    Waiting,
    /// Compare these emojis (symbol, name) with the other screen.
    Emojis(Vec<(String, String)>),
    /// We said they match; waiting for the other side.
    Confirmed,
    Done,
    Cancelled(String),
}

/// Whether we've verified `user` (`None`: they have no cross-signing keys,
/// so there's nothing to verify yet).
pub async fn is_verified(client: &Client, user: &UserId) -> Option<bool> {
    client.encryption().request_user_identity(user).await.ok().flatten().map(|i| i.is_verified())
}

/// Starts verifying `user` (our own user id verifies this device against
/// our other sessions).
pub async fn start(client: &Client, tx: &broadcast::Sender<ClientEvent>, account: AccountId, user: &UserId) -> Result<()> {
    let identity = client
        .encryption()
        .request_user_identity(user)
        .await?
        .ok_or_else(|| anyhow!("{user} hasn't set up encryption keys (cross-signing) yet, so there's nothing to verify"))?;
    let request = identity.request_verification().await?;
    let _ = tx.send(event(account, user, request.flow_id(), VerifyState::Waiting));
    tokio::spawn(watch_request(request, tx.clone(), account));
    Ok(())
}

/// Accepts an incoming request.
pub async fn accept(client: &Client, tx: &broadcast::Sender<ClientEvent>, account: AccountId, user: &UserId, flow_id: &str) -> Result<()> {
    let request = client.encryption().get_verification_request(user, flow_id).await.ok_or_else(|| anyhow!("that request has expired"))?;
    request.accept().await?;
    let _ = tx.send(event(account, user, flow_id, VerifyState::Waiting));
    tokio::spawn(watch_request(request, tx.clone(), account));
    Ok(())
}

/// Answers the emoji comparison.
pub async fn confirm(client: &Client, user: &UserId, flow_id: &str, matches: bool) -> Result<()> {
    let sas = client
        .encryption()
        .get_verification(user, flow_id)
        .await
        .and_then(|v| v.sas())
        .ok_or_else(|| anyhow!("that verification has ended"))?;
    if matches { sas.confirm().await? } else { sas.mismatch().await? }
    Ok(())
}

pub async fn cancel(client: &Client, user: &UserId, flow_id: &str) -> Result<()> {
    if let Some(sas) = client.encryption().get_verification(user, flow_id).await.and_then(|v| v.sas()) {
        sas.cancel().await?;
    } else if let Some(req) = client.encryption().get_verification_request(user, flow_id).await {
        req.cancel().await?;
    } else {
        bail!("that verification has already ended");
    }
    Ok(())
}

fn event(account: AccountId, user: &UserId, flow_id: &str, state: VerifyState) -> ClientEvent {
    ClientEvent::Verification { account, user: user.to_owned(), flow_id: flow_id.to_owned(), state }
}

/// Follows a request until it turns into an emoji comparison, finishes or
/// is cancelled.
async fn watch_request(request: VerificationRequest, tx: broadcast::Sender<ClientEvent>, account: AccountId) {
    let user: OwnedUserId = request.other_user_id().to_owned();
    let flow = request.flow_id().to_owned();
    let mut states = request.changes();
    // The state may already have moved on before we subscribed.
    let mut next = Some(request.state());
    loop {
        let Some(state) = next.take() else {
            match states.next().await {
                Some(s) => next = Some(s),
                None => return,
            }
            continue;
        };
        match state {
            VerificationRequestState::Ready { .. } if request.we_started() => match request.start_sas().await {
                Ok(Some(sas)) => return watch_sas(sas, flow, tx, account).await,
                Ok(None) => {
                    let _ = tx.send(event(account, &user, &flow, VerifyState::Cancelled("their device can't do emoji verification".into())));
                    return;
                }
                Err(e) => {
                    let _ = tx.send(event(account, &user, &flow, VerifyState::Cancelled(e.to_string())));
                    return;
                }
            },
            VerificationRequestState::Transitioned { verification } => {
                if let Some(sas) = verification.sas() {
                    return watch_sas(sas, flow, tx, account).await;
                }
            }
            VerificationRequestState::Done => {
                let _ = tx.send(event(account, &user, &flow, VerifyState::Done));
                return;
            }
            VerificationRequestState::Cancelled(info) => {
                let _ = tx.send(event(account, &user, &flow, VerifyState::Cancelled(info.reason().to_owned())));
                return;
            }
            _ => {}
        }
    }
}

async fn watch_sas(sas: SasVerification, flow: String, tx: broadcast::Sender<ClientEvent>, account: AccountId) {
    let user: OwnedUserId = sas.other_user_id().to_owned();
    let mut states = sas.changes();
    let mut next = Some(sas.state());
    loop {
        let Some(state) = next.take() else {
            match states.next().await {
                Some(s) => next = Some(s),
                None => return,
            }
            continue;
        };
        let ui = match state {
            // The other side started the comparison: agree to it.
            SasState::Started { .. } => {
                if let Err(e) = sas.accept().await {
                    Some(VerifyState::Cancelled(e.to_string()))
                } else {
                    None
                }
            }
            SasState::KeysExchanged { emojis: Some(e), .. } => Some(VerifyState::Emojis(
                e.emojis.iter().map(|x| (x.symbol.to_owned(), x.description.to_owned())).collect(),
            )),
            SasState::KeysExchanged { emojis: None, .. } => Some(VerifyState::Cancelled("their device only offers number comparison".into())),
            SasState::Confirmed => Some(VerifyState::Confirmed),
            SasState::Done { .. } => Some(VerifyState::Done),
            SasState::Cancelled(info) => Some(VerifyState::Cancelled(info.reason().to_owned())),
            _ => None,
        };
        if let Some(s) = ui {
            let end = matches!(s, VerifyState::Done | VerifyState::Cancelled(_));
            let _ = tx.send(event(account, &user, &flow, s));
            if end {
                return;
            }
        }
    }
}

/// The server's "confirm it's you" answer, with the account password.
fn password_auth(client: &Client, password: &str, session: Option<String>) -> Result<matrix_sdk::ruma::api::client::uiaa::AuthData> {
    use matrix_sdk::ruma::api::client::uiaa::{AuthData, Password};
    let user = client.user_id().ok_or_else(|| anyhow!("not signed in"))?.to_owned();
    let mut p = Password::new((&user).into(), password.to_owned());
    p.session = session;
    Ok(AuthData::Password(p))
}

/// Gives an account that has no cross-signing keys yet (a new account) a
/// set, made on this device, which makes this session verified without
/// needing another one. Accounts that already have keys are left alone.
/// `Ok(false)`: the server wants the password and none was given.
pub async fn ensure_identity(client: &Client, password: Option<&str>) -> Result<bool> {
    let enc = client.encryption();
    let Err(e) = enc.bootstrap_cross_signing_if_needed(None).await else { return Ok(true) };
    let Some(info) = e.as_uiaa_response() else { return Err(e.into()) };
    let Some(pw) = password else { return Ok(false) };
    enc.bootstrap_cross_signing_if_needed(Some(password_auth(client, pw, info.session.clone())?)).await?;
    Ok(true)
}

/// Whether this device already knows the account's cross-signing keys,
/// from its own store: no network, so it's instant. `None` when it
/// doesn't know (then [`has_identity`] asks the server).
pub async fn known_identity(client: &Client) -> Option<bool> {
    let me = client.user_id()?;
    match client.encryption().get_user_identity(me).await {
        Ok(Some(_)) => Some(true),
        _ => None,
    }
}

/// Whether the account has cross-signing keys on the server at all. Asks
/// the server every time (a keys query), at most 10 seconds.
pub async fn has_identity(client: &Client) -> bool {
    if known_identity(client).await == Some(true) {
        return true;
    }
    let Some(me) = client.user_id() else { return false };
    let enc = client.encryption();
    let ask = enc.request_user_identity(me);
    matches!(tokio::time::timeout(std::time::Duration::from_secs(10), ask).await, Ok(Ok(Some(_))))
}

/// Replaces the account's cross-signing keys with new ones made here, for
/// when no other session or recovery key is left. Other sessions and
/// people who verified you have to verify again, and the server-side key
/// backup is deleted.
pub async fn reset_identity(client: &Client, password: &str) -> Result<()> {
    use matrix_sdk::encryption::CrossSigningResetAuthType;
    let Some(handle) = client.encryption().recovery().reset_identity().await? else { return Ok(()) };
    let auth = match handle.auth_type() {
        CrossSigningResetAuthType::Uiaa(info) => Some(password_auth(client, password, info.session.clone())?),
        CrossSigningResetAuthType::OAuth(o) => bail!("approve the reset at {} first, then try again", o.approval_url),
    };
    handle.reset(auth).await?;
    Ok(())
}

/// Turns on the recovery key (secret storage plus key backup) and returns
/// it, for the user to write down. `None`: it's already on.
pub async fn enable_recovery(client: &Client) -> Result<Option<String>> {
    use matrix_sdk::encryption::recovery::RecoveryState;
    let r = client.encryption().recovery();
    match r.state() {
        RecoveryState::Enabled => Ok(None),
        RecoveryState::Incomplete => bail!("a recovery key already exists for this account; enter it under \"Read older encrypted messages\""),
        _ => Ok(Some(r.enable().wait_for_backups_to_upload().await?)),
    }
}

/// Whether a recovery key is set up and unlocked on this session.
pub fn recovery_on(client: &Client) -> bool {
    client.encryption().recovery().state() == matrix_sdk::encryption::recovery::RecoveryState::Enabled
}

/// Marks `other` verified by `client`'s user, without the emoji check:
/// for your own accounts on this device, which can't fool each other.
/// Needs this session to hold the user-signing key.
pub async fn vouch(client: &Client, other: &UserId) -> Result<()> {
    let can = client.encryption().cross_signing_status().await.is_some_and(|s| s.has_user_signing);
    if !can || client.user_id() == Some(other) {
        return Ok(());
    }
    if let Some(id) = client.encryption().request_user_identity(other).await?
        && !id.is_verified()
    {
        id.verify().await?;
    }
    Ok(())
}
