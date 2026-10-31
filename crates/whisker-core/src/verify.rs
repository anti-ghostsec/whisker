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
