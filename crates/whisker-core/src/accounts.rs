//! Every account, on every server, logged in at once.
//!
//! Accounts live in vaults. A data directory can hold several vault files,
//! each opened by its own passphrase, so hidden alts can sit in a vault that
//! the main passphrase doesn't open: until it's unlocked, nothing on screen
//! (or in the main vault) says those accounts exist.

use crate::events::ClientEvent;
use crate::profile::{AccountId, AccountKind, AccountRecord, Profile, normalize_tag};
use crate::vault::{Vault, VaultError, random_secret};
use anyhow::{Context, Result, anyhow, bail};
use matrix_sdk::config::SyncSettings;
use crate::timeline::{self, HistoryItem};
use matrix_sdk::ruma::api::client::filter::{FilterDefinition, LazyLoadOptions};
use matrix_sdk::ruma::events::room::encrypted::OriginalSyncRoomEncryptedEvent;
use matrix_sdk::ruma::events::room::message::OriginalSyncRoomMessageEvent;
use matrix_sdk::ruma::events::room::redaction::OriginalSyncRoomRedactionEvent;
use matrix_sdk::ruma::{OwnedMxcUri, OwnedRoomId, UInt, UserId};
use matrix_sdk::{Client, LoopCtrl, Room};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};
use tokio::task::JoinHandle;

/// Which vault file an account came from (its file stem).
pub type VaultId = String;

struct OpenVault {
    vault: Vault,
    profile: Profile,
}

struct LiveAccount {
    record: AccountRecord,
    /// Display name and avatar from the server profile (not stored in the vault).
    profile_name: Option<String>,
    avatar: Option<OwnedMxcUri>,
    vault: VaultId,
    client: Client,
    sync: Option<JoinHandle<()>>,
}

impl Drop for LiveAccount {
    fn drop(&mut self) {
        if let Some(h) = self.sync.take() {
            h.abort();
        }
    }
}

/// A snapshot of one account for the UI.
#[derive(Debug, Clone)]
pub struct AccountSummary {
    pub id: AccountId,
    pub vault: VaultId,
    pub kind: AccountKind,
    pub user_id: String,
    pub homeserver: String,
    pub display_name: String,
    pub avatar: Option<OwnedMxcUri>,
    /// Display name on the server (what others see), if set.
    pub profile_name: Option<String>,
    pub tags: BTreeSet<String>,
    pub position: u32,
}

#[derive(Default)]
struct Inner {
    vaults: HashMap<VaultId, OpenVault>,
    live: HashMap<AccountId, LiveAccount>,
}

/// Newest message per room, for room-list previews and ordering.
type Latest = HashMap<(AccountId, OwnedRoomId), HistoryItem>;

#[derive(Clone)]
pub struct AccountManager {
    data_dir: PathBuf,
    inner: Arc<RwLock<Inner>>,
    latest: Arc<std::sync::Mutex<Latest>>,
    events: broadcast::Sender<ClientEvent>,
    /// Device name shown to servers and other users. Deliberately generic.
    device_name: Arc<str>,
}

pub struct LoginRequest<'a> {
    pub vault: &'a str,
    /// Create the account instead of signing in to an existing one.
    pub register: bool,
    /// Registration token, for servers that hand them out to new members.
    pub token: Option<&'a str>,
    pub homeserver: &'a str,
    pub username: &'a str,
    pub password: &'a str,
    pub kind: AccountKind,
    pub tags: BTreeSet<String>,
    pub proxy: Option<String>,
}

impl AccountManager {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        let (events, _) = broadcast::channel(1024);
        Self {
            data_dir: data_dir.into(),
            inner: Arc::default(),
            latest: Arc::default(),
            events,
            device_name: "Whisker".into(),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// For code that reports progress into the shared event stream.
    pub fn event_sender(&self) -> broadcast::Sender<ClientEvent> {
        self.events.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ClientEvent> {
        self.events.subscribe()
    }

    fn vaults_dir(&self) -> PathBuf {
        self.data_dir.join("vaults")
    }

    fn store_dir(&self, id: AccountId) -> PathBuf {
        self.data_dir.join("stores").join(id.to_string())
    }

    /// True if no vault exists yet (first run).
    pub fn is_empty(&self) -> bool {
        std::fs::read_dir(self.vaults_dir())
            .map(|mut d| d.next().is_none())
            .unwrap_or(true)
    }

    /// Creates a new vault under `passphrase` and keeps it open.
    pub async fn create_vault(&self, passphrase: &str) -> Result<VaultId> {
        let id = random_secret()[..16].to_string();
        let path = self.vaults_dir().join(format!("{id}.vault"));
        let pass = passphrase.to_owned();
        let profile = Profile::default();
        let vault = tokio::task::spawn_blocking(move || -> Result<Vault, VaultError> {
            let v = Vault::create(path, &pass)?;
            v.save(&Profile::default())?;
            Ok(v)
        })
        .await??;
        self.inner.write().await.vaults.insert(id.clone(), OpenVault { vault, profile });
        Ok(id)
    }

    /// Tries `passphrase` against every vault that isn't open yet, restores
    /// the accounts of each one it opens, and starts syncing them.
    /// Returns the ids of the vaults it opened (empty means a wrong passphrase).
    pub async fn unlock(&self, passphrase: &str) -> Result<Vec<VaultId>> {
        let already: BTreeSet<VaultId> = self.inner.read().await.vaults.keys().cloned().collect();
        let mut files = Vec::new();
        if let Ok(dir) = std::fs::read_dir(self.vaults_dir()) {
            for e in dir.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "vault") {
                    let stem = p.file_stem().unwrap().to_string_lossy().into_owned();
                    if !already.contains(&stem) {
                        files.push((stem, p));
                    }
                }
            }
        }

        let pass = passphrase.to_owned();
        let opened = tokio::task::spawn_blocking(move || {
            files
                .into_iter()
                .filter_map(|(id, p)| match Vault::open::<Profile>(p, &pass) {
                    Ok((v, prof)) => Some((id, v, prof)),
                    Err(VaultError::Decrypt) => None,
                    Err(e) => {
                        tracing::warn!("skipping vault {id}: {e}");
                        None
                    }
                })
                .collect::<Vec<_>>()
        })
        .await?;

        let mut ids = Vec::new();
        for (id, vault, profile) in opened {
            let records = profile.accounts.clone();
            self.inner.write().await.vaults.insert(id.clone(), OpenVault { vault, profile });
            // All at once: one slow account shouldn't hold up the rest.
            futures_util::future::join_all(records.into_iter().map(|rec| {
                let id = &id;
                async move {
                    if let Err(e) = self.restore(id, rec.clone()).await {
                        tracing::warn!("could not restore {}: {e:#}", rec.user_id);
                        let _ = self.events.send(ClientEvent::AccountError {
                            account: rec.id,
                            message: format!("{e:#}"),
                        });
                    }
                }
            }))
            .await;
            ids.push(id);
        }
        Ok(ids)
    }

    /// Stops every account in `vault` and forgets its key. Its accounts
    /// vanish from the UI until the vault is unlocked again.
    pub async fn lock_vault(&self, vault: &str) {
        let mut inner = self.inner.write().await;
        inner.live.retain(|_, a| a.vault != vault);
        inner.vaults.remove(vault);
        let _ = self.events.send(ClientEvent::AccountsChanged);
    }

    pub async fn lock_all(&self) {
        let mut inner = self.inner.write().await;
        inner.live.clear();
        inner.vaults.clear();
        let _ = self.events.send(ClientEvent::AccountsChanged);
    }

    pub async fn open_vaults(&self) -> Vec<VaultId> {
        self.inner.read().await.vaults.keys().cloned().collect()
    }

    /// `known`: `homeserver` is the URL a login already resolved, so skip
    /// discovery. Discovery is network round trips, and doing them for every
    /// account on unlock left it hanging whenever a server was slow.
    async fn build_client(&self, id: AccountId, homeserver: &str, known: bool, store_pass: &str, proxy: Option<&str>) -> Result<Client> {
        let mut b = Client::builder();
        b = if known { b.homeserver_url(homeserver) } else { b.server_name_or_homeserver_url(homeserver) };
        let mut b = b.sqlite_store(self.store_dir(id), Some(store_pass));
        if let Some(p) = proxy {
            b = b.proxy(p);
        }
        Ok(b.build().await?)
    }

    async fn restore(&self, vault: &str, mut record: AccountRecord) -> Result<()> {
        // Servers move (the saved URL can go dead), so ask the server name
        // where it lives now, but don't let a slow answer hold up unlocking.
        if let Some(now) = discover(&record).await
            && now != record.homeserver.trim_end_matches('/')
        {
            tracing::info!("{} moved from {} to {now}", record.user_id, record.homeserver);
            record.homeserver = now;
            let (id, hs) = (record.id, record.homeserver.clone());
            self.mutate_profile(vault, |p| {
                if let Some(r) = p.accounts.iter_mut().find(|r| r.id == id) {
                    r.homeserver = hs;
                }
            })
            .await?;
        }
        let client = self
            .build_client(record.id, &record.homeserver, true, &record.store_passphrase, record.proxy.as_deref())
            .await?;
        client.restore_session(record.session.clone()).await?;
        self.go_live(vault, record, client).await;
        Ok(())
    }

    async fn go_live(&self, vault: &str, record: AccountRecord, client: Client) {
        let account = record.id;

        let (tx, latest) = (self.events.clone(), self.latest.clone());
        client.add_event_handler(move |ev: OriginalSyncRoomMessageEvent, room: Room| {
            let (tx, latest) = (tx.clone(), latest.clone());
            async move {
                if let matrix_sdk::ruma::events::room::message::MessageType::VerificationRequest(r) = &ev.content.msgtype {
                    if room.own_user_id() == r.to && ev.sender != r.to {
                        let _ = tx.send(ClientEvent::Verification {
                            account,
                            user: ev.sender.clone(),
                            flow_id: ev.event_id.to_string(),
                            state: crate::verify::VerifyState::Incoming { self_verification: false },
                        });
                    }
                    return;
                }
                match timeline::parse(&ev.content) {
                    timeline::Parsed::Edit { target, body } => {
                        let _ = tx.send(ClientEvent::Edited { account, room_id: room.room_id().to_owned(), target, body });
                    }
                    timeline::Parsed::New { body, is_text, reply_to, media } => {
                        let m = Msg { body, is_text, reply_to, media };
                        publish(&tx, &latest, account, &room, ev.event_id, &ev.sender, m, ev.origin_server_ts).await;
                    }
                }
            }
        });
        // Someone wants to verify with us (to-device: usually our own other sessions).
        let tx = self.events.clone();
        client.add_event_handler(move |ev: matrix_sdk::ruma::events::key::verification::request::ToDeviceKeyVerificationRequestEvent, c: Client| {
            let tx = tx.clone();
            async move {
                let me = c.user_id().map(ToOwned::to_owned);
                let _ = tx.send(ClientEvent::Verification {
                    account,
                    flow_id: ev.content.transaction_id.to_string(),
                    state: crate::verify::VerifyState::Incoming { self_verification: me.as_ref() == Some(&ev.sender) },
                    user: ev.sender,
                });
            }
        });
        let tx = self.events.clone();
        client.add_event_handler(move |ev: OriginalSyncRoomRedactionEvent, room: Room| {
            let tx = tx.clone();
            async move {
                if let Some(target) = ev.content.redacts.clone().or(ev.redacts.clone()) {
                    let _ = tx.send(ClientEvent::Redacted { account, room_id: room.room_id().to_owned(), target });
                }
            }
        });
        // Messages we can't decrypt (yet) still show up, labelled.
        let (tx, latest) = (self.events.clone(), self.latest.clone());
        client.add_event_handler(move |ev: OriginalSyncRoomEncryptedEvent, room: Room| {
            let (tx, latest) = (tx.clone(), latest.clone());
            async move {
                let m = Msg { body: timeline::UNDECRYPTABLE.into(), is_text: false, reply_to: None, media: None };
                publish(&tx, &latest, account, &room, ev.event_id, &ev.sender, m, ev.origin_server_ts).await;
            }
        });

        let sync_client = client.clone();
        let tx = self.events.clone();
        let sync = tokio::spawn(async move {
            // Lazy-load members: the first sync of a big account stays small and fast.
            let mut filter = FilterDefinition::default();
            filter.room.state.lazy_load_options = LazyLoadOptions::Enabled { include_redundant_members: false };
            filter.room.timeline.lazy_load_options = LazyLoadOptions::Enabled { include_redundant_members: false };
            filter.room.timeline.limit = Some(UInt::from(20u32));
            let settings = SyncSettings::default().filter(filter.into());
            // `sync_with_callback` only returns on an error; back off and keep going.
            let mut delay = 1u64;
            loop {
                let tx2 = tx.clone();
                let res = sync_client
                    .sync_with_callback(settings.clone(), move |_| {
                        let tx = tx2.clone();
                        async move {
                            let _ = tx.send(ClientEvent::Synced { account });
                            LoopCtrl::Continue
                        }
                    })
                    .await;
                if let Err(e) = res {
                    let _ = tx.send(ClientEvent::AccountError { account, message: e.to_string() });
                }
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                delay = (delay * 2).min(60);
            }
        });

        self.inner.write().await.live.insert(
            account,
            LiveAccount { record, profile_name: None, avatar: None, vault: vault.to_owned(), client: client.clone(), sync: Some(sync) },
        );
        let _ = self.events.send(ClientEvent::AccountsChanged);

        // Fetch the profile in the background; the list updates when it lands.
        let (mgr, c) = (self.clone(), client);
        tokio::spawn(async move {
            let acc = c.account();
            let name = acc.get_display_name().await.ok().flatten();
            let avatar = acc.get_avatar_url().await.ok().flatten();
            if let Some(l) = mgr.inner.write().await.live.get_mut(&account) {
                l.profile_name = name;
                l.avatar = avatar;
            }
            let _ = mgr.events.send(ClientEvent::AccountsChanged);
        });
    }

    /// Re-reads an account's server profile (after changing it).
    pub async fn refresh_profile(&self, account: AccountId) {
        let Some(c) = self.client(account).await else { return };
        let acc = c.account();
        let name = acc.get_display_name().await.ok().flatten();
        let avatar = acc.get_avatar_url().await.ok().flatten();
        if let Some(l) = self.inner.write().await.live.get_mut(&account) {
            l.profile_name = name;
            l.avatar = avatar;
        }
        let _ = self.events.send(ClientEvent::AccountsChanged);
    }

    /// The newest message we've seen in `room` (from sync or history).
    pub fn latest(&self, account: AccountId, room: &OwnedRoomId) -> Option<HistoryItem> {
        self.latest.lock().unwrap().get(&(account, room.clone())).cloned()
    }

    /// Logs in a new account and saves it into `req.vault`.
    pub async fn login(&self, req: LoginRequest<'_>) -> Result<AccountId> {
        if !self.inner.read().await.vaults.contains_key(req.vault) {
            bail!("vault is locked");
        }
        let id = AccountId::new();
        let store_passphrase = random_secret();
        let client = self
            .build_client(id, req.homeserver, false, &store_passphrase, req.proxy.as_deref())
            .await
            .context("could not reach homeserver")?;
        if req.register {
            self.register_on(&client, req.username, req.password, req.token).await?;
        } else {
            client
                .matrix_auth()
                .login_username(req.username, req.password)
                .initial_device_display_name(&self.device_name)
                .send()
                .await
                .context("login failed")?;
        }
        let session = client.matrix_auth().session().ok_or_else(|| anyhow!("no session after login"))?;
        let position = self.inner.read().await.live.values().map(|a| a.record.position).max().map_or(0, |p| p + 1);

        let record = AccountRecord {
            id,
            kind: req.kind,
            user_id: session.meta.user_id.to_string(),
            homeserver: client.homeserver().to_string(),
            label: None,
            tags: req.tags.iter().filter_map(|t| normalize_tag(t)).collect(),
            proxy: req.proxy,
            session,
            store_passphrase,
            position,
        };
        self.mutate_profile(req.vault, |p| p.accounts.push(record.clone())).await?;
        self.go_live(req.vault, record, client).await;
        Ok(id)
    }

    /// Creates an account, answering the server's sign-up checks that can be
    /// done in-app (none, or a registration token). Servers that want an
    /// email, phone number or captcha get a clear error instead.
    async fn register_on(&self, client: &Client, username: &str, password: &str, token: Option<&str>) -> Result<()> {
        use matrix_sdk::ruma::api::client::account::register::v3::Request;
        use matrix_sdk::ruma::api::client::uiaa::{AuthData, AuthType, Dummy, RegistrationToken};

        let mut auth: Option<AuthData> = None;
        for _ in 0..5 {
            let mut req = Request::new();
            req.username = Some(username.to_owned());
            req.password = Some(password.to_owned());
            req.initial_device_display_name = Some(self.device_name.to_string());
            req.auth = auth.take();
            let err = match client.matrix_auth().register(req).await {
                Ok(_) => return Ok(()),
                Err(e) => e,
            };
            let Some(info) = err.as_uiaa_response() else {
                return Err(anyhow!("couldn't create the account: {err}"));
            };
            let doable = |t: &AuthType| matches!(t, AuthType::Dummy) || (matches!(t, AuthType::RegistrationToken) && token.is_some());
            let Some(flow) = info.flows.iter().find(|f| f.stages.iter().all(doable)) else {
                let needs: BTreeSet<String> = info.flows.iter().flat_map(|f| f.stages.iter().map(|s| match s {
                    AuthType::EmailIdentity => "an email address".to_owned(),
                    AuthType::Msisdn => "a phone number".to_owned(),
                    AuthType::ReCaptcha => "a captcha".to_owned(),
                    AuthType::RegistrationToken => "a registration token".to_owned(),
                    AuthType::Terms => "accepting its terms".to_owned(),
                    other => other.to_string(),
                })).collect();
                let needs: Vec<_> = needs.into_iter().collect();
                bail!(
                    "this server's sign-up needs {}. Enter a registration token if you have one, or sign up on the server's website and then log in here",
                    needs.join(" or ")
                );
            };
            let Some(next) = flow.stages.iter().find(|s| !info.completed.contains(s)) else {
                bail!("the server didn't accept the sign-up");
            };
            let session = info.session.clone();
            auth = Some(match next {
                AuthType::RegistrationToken => {
                    let mut t = RegistrationToken::new(token.unwrap_or_default().to_owned());
                    t.session = session;
                    AuthData::RegistrationToken(t)
                }
                _ => {
                    let mut d = Dummy::new();
                    d.session = session;
                    AuthData::Dummy(d)
                }
            });
        }
        bail!("the server kept asking for more sign-up steps")
    }

    /// Moves an account one place up (`-1`) or down (`1`) in the list.
    pub async fn move_account(&self, id: AccountId, delta: i32) -> Result<()> {
        let order: Vec<AccountId> = self.accounts().await.into_iter().map(|a| a.id).collect();
        let Some(i) = order.iter().position(|a| *a == id) else { bail!("no such account") };
        let j = (i as i64 + delta as i64).clamp(0, order.len() as i64 - 1) as usize;
        if i == j {
            return Ok(());
        }
        let mut order = order;
        let moved = order.remove(i);
        order.insert(j, moved);
        for (pos, acc) in order.iter().enumerate() {
            let pos = pos as u32;
            self.mutate_account_quiet(*acc, |r| r.position = pos).await?;
        }
        let _ = self.events.send(ClientEvent::AccountsChanged);
        Ok(())
    }

    /// Logs the account out on the server, removes it from its vault and
    /// deletes its local store.
    /// Takes effect locally at once; the server is told in the background
    /// (it used to wait on the server, and a second click then found nothing
    /// to log out). Logging out an account that's already gone is a no-op.
    pub async fn logout(&self, id: AccountId) -> Result<()> {
        let (live, vault) = {
            let mut inner = self.inner.write().await;
            let live = inner.live.remove(&id);
            let vault = live.as_ref().map(|l| l.vault.clone()).or_else(|| {
                inner.vaults.iter().find(|(_, v)| v.profile.accounts.iter().any(|a| a.id == id)).map(|(k, _)| k.clone())
            });
            (live, vault)
        };
        if let Some(vault) = vault {
            self.mutate_profile(&vault, |p| p.accounts.retain(|a| a.id != id)).await?;
        }
        let _ = self.events.send(ClientEvent::AccountsChanged);
        let store = self.store_dir(id);
        tokio::spawn(async move {
            if let Some(live) = live {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(20), live.client.logout()).await;
                drop(live);
            }
            let _ = tokio::fs::remove_dir_all(store).await;
        });
        Ok(())
    }

    async fn mutate_profile(&self, vault: &str, f: impl FnOnce(&mut Profile)) -> Result<()> {
        let mut inner = self.inner.write().await;
        let ov = inner.vaults.get_mut(vault).ok_or_else(|| anyhow!("vault is locked"))?;
        f(&mut ov.profile);
        ov.vault.save(&ov.profile)?;
        Ok(())
    }

    async fn mutate_account(&self, id: AccountId, f: impl Fn(&mut AccountRecord)) -> Result<()> {
        self.mutate_account_quiet(id, f).await?;
        let _ = self.events.send(ClientEvent::AccountsChanged);
        Ok(())
    }

    async fn mutate_account_quiet(&self, id: AccountId, f: impl Fn(&mut AccountRecord)) -> Result<()> {
        let vault = {
            let mut inner = self.inner.write().await;
            let live = inner.live.get_mut(&id).ok_or_else(|| anyhow!("no such account"))?;
            f(&mut live.record);
            live.vault.clone()
        };
        self.mutate_profile(&vault, |p| {
            if let Some(r) = p.accounts.iter_mut().find(|r| r.id == id) {
                f(r);
            }
        })
        .await
    }

    pub async fn set_tags(&self, id: AccountId, tags: impl IntoIterator<Item = String>) -> Result<()> {
        let tags: BTreeSet<String> = tags.into_iter().filter_map(|t| normalize_tag(&t)).collect();
        self.mutate_account(id, |r| r.tags = tags.clone()).await
    }

    /// Takes effect the next time the vault is unlocked.
    pub async fn set_proxy(&self, id: AccountId, proxy: Option<String>) -> Result<()> {
        let proxy = proxy.map(|p| p.trim().to_owned()).filter(|p| !p.is_empty());
        self.mutate_account(id, |r| r.proxy = proxy.clone()).await
    }

    /// Label and proxy for the settings page.
    pub async fn details(&self, id: AccountId) -> Option<(Option<String>, Option<String>)> {
        let inner = self.inner.read().await;
        let r = &inner.live.get(&id)?.record;
        Some((r.label.clone(), r.proxy.clone()))
    }

    /// Re-encrypts `vault` under a new passphrase. The old one must be right.
    pub async fn change_passphrase(&self, vault: &str, old: &str, new: &str) -> Result<()> {
        if new.len() < 8 {
            bail!("use at least 8 characters");
        }
        let path = self.vaults_dir().join(format!("{vault}.vault"));
        let (old, new_) = (old.to_owned(), new.to_owned());
        // Prove the old passphrase before replacing anything.
        tokio::task::spawn_blocking(move || Vault::open::<Profile>(path, &old).map(|_| ()))
            .await?
            .map_err(|e| match e {
                VaultError::Decrypt => anyhow!("current passphrase is wrong"),
                e => e.into(),
            })?;
        let mut inner = self.inner.write().await;
        let ov = inner.vaults.get_mut(vault).ok_or_else(|| anyhow!("vault is locked"))?;
        let profile = ov.profile.clone();
        let vault_ref = &mut ov.vault;
        tokio::task::block_in_place(|| vault_ref.change_passphrase(&new_, &profile))?;
        Ok(())
    }

    pub async fn set_label(&self, id: AccountId, label: Option<String>) -> Result<()> {
        let label = label.filter(|l| !l.trim().is_empty());
        self.mutate_account(id, |r| r.label = label.clone()).await
    }

    pub async fn accounts(&self) -> Vec<AccountSummary> {
        let inner = self.inner.read().await;
        let mut v: Vec<_> = inner
            .live
            .values()
            .map(|a| AccountSummary {
                id: a.record.id,
                vault: a.vault.clone(),
                kind: a.record.kind,
                user_id: a.record.user_id.clone(),
                homeserver: a.record.homeserver.clone(),
                display_name: a
                    .record
                    .label
                    .clone()
                    .or_else(|| a.profile_name.clone())
                    .unwrap_or_else(|| a.record.user_id.clone()),
                avatar: a.avatar.clone(),
                profile_name: a.profile_name.clone(),
                position: a.record.position,
                tags: a.record.tags.clone(),
            })
            .collect();
        v.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.display_name.cmp(&b.display_name)));
        v
    }

    /// Accounts carrying `tag` (all accounts when `tag` is `None`).
    pub async fn accounts_tagged(&self, tag: Option<&str>) -> Vec<AccountSummary> {
        let tag = tag.and_then(normalize_tag);
        self.accounts()
            .await
            .into_iter()
            .filter(|a| tag.as_ref().is_none_or(|t| a.tags.contains(t)))
            .collect()
    }

    /// Every tag in use across unlocked accounts.
    pub async fn all_tags(&self) -> BTreeSet<String> {
        self.inner.read().await.live.values().flat_map(|a| a.record.tags.iter().cloned()).collect()
    }

    pub async fn client(&self, id: AccountId) -> Option<Client> {
        self.inner.read().await.live.get(&id).map(|a| a.client.clone())
    }

    /// Reads typed data another crate keeps in a vault (see [`Profile::extras`]).
    pub async fn extra<T: serde::de::DeserializeOwned>(&self, vault: &str, key: &str) -> Option<T> {
        let inner = self.inner.read().await;
        let v = inner.vaults.get(vault)?.profile.extras.get(key)?.clone();
        serde_json::from_value(v).ok()
    }

    pub async fn set_extra<T: serde::Serialize>(&self, vault: &str, key: &str, value: &T) -> Result<()> {
        let v = serde_json::to_value(value)?;
        self.mutate_profile(vault, |p| {
            p.extras.insert(key.to_owned(), v);
        })
        .await
    }
}

struct Msg {
    body: String,
    is_text: bool,
    reply_to: Option<matrix_sdk::ruma::OwnedEventId>,
    media: Option<timeline::Media>,
}

#[allow(clippy::too_many_arguments)]
async fn publish(
    tx: &broadcast::Sender<ClientEvent>,
    latest: &std::sync::Mutex<Latest>,
    account: AccountId,
    room: &Room,
    event_id: matrix_sdk::ruma::OwnedEventId,
    sender: &UserId,
    msg: Msg,
    ts: matrix_sdk::ruma::MilliSecondsSinceUnixEpoch,
) {
    let Msg { body, is_text, reply_to, media } = msg;
    let (sender_name, sender_avatar) = timeline::member_profile(room, sender).await;
    let item = HistoryItem {
        event_id: event_id.clone(),
        sender: sender.to_owned(),
        sender_name: sender_name.clone(),
        sender_avatar: sender_avatar.clone(),
        body: body.clone(),
        ts,
        reply_to: reply_to.clone(),
        media: media.clone(),
        edited: false,
    };
    {
        let mut l = latest.lock().unwrap();
        let key = (account, room.room_id().to_owned());
        if l.get(&key).is_none_or(|old| old.ts <= ts) {
            l.insert(key, item);
        }
    }
    let _ = tx.send(ClientEvent::Message {
        account,
        room_id: room.room_id().to_owned(),
        event_id,
        sender: sender.to_owned(),
        sender_name,
        sender_avatar,
        body,
        origin_server_ts: ts,
        is_text,
        reply_to,
        media: media.map(Box::new),
    });
}

/// Rooms for `account`, newest activity first.
impl AccountManager {
    pub async fn rooms(&self, account: AccountId) -> Vec<timeline::RoomSummary> {
        let Some(c) = self.client(account).await else { return Vec::new() };
        let mut rooms = timeline::rooms(&c).await;
        let latest = self.latest.lock().unwrap();
        for r in &mut rooms {
            r.latest = latest.get(&(account, r.room_id.clone())).cloned();
        }
        drop(latest);
        rooms.sort_by(|a, b| {
            b.invite
                .cmp(&a.invite)
                .then_with(|| b.pinned.cmp(&a.pinned))
                .then_with(|| b.latest.as_ref().map(|l| l.ts).cmp(&a.latest.as_ref().map(|l| l.ts)))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        rooms
    }

    /// Marks a room read up to its newest message. Uses a *private* read
    /// receipt: the unread count clears, but nobody else learns you read it.
    pub async fn mark_read(&self, account: AccountId, room_id: &OwnedRoomId) -> Result<()> {
        use matrix_sdk::ruma::api::client::receipt::create_receipt::v3::ReceiptType;
        use matrix_sdk::ruma::events::receipt::ReceiptThread;
        let c = self.client(account).await.ok_or_else(|| anyhow!("no such account"))?;
        let room = c.get_room(room_id).ok_or_else(|| anyhow!("no such room"))?;
        let latest = self.latest.lock().unwrap().get(&(account, room_id.clone())).map(|l| l.event_id.clone());
        if let Some(ev) = latest {
            room.send_single_receipt(ReceiptType::ReadPrivate, ReceiptThread::Unthreaded, ev).await?;
        }
        if room.is_marked_unread() {
            room.set_unread_flag(false).await?;
        }
        Ok(())
    }

    /// Unlocks server-side key backup so older encrypted messages can be read.
    pub async fn recover(&self, account: AccountId, recovery_key: &str) -> Result<()> {
        let c = self.client(account).await.ok_or_else(|| anyhow!("no such account"))?;
        c.encryption().recovery().recover(recovery_key.trim()).await?;
        Ok(())
    }

    /// Remembers the newest message from a history fetch, so previews work
    /// for rooms that were quiet since the last sync.
    /// Fetches the newest messages of rooms with no preview yet (right after
    /// an unlock nothing has arrived live), a few rooms at a time.
    pub async fn fill_previews(&self, account: AccountId) {
        use futures_util::StreamExt;
        let Some(c) = self.client(account).await else { return };
        let missing: Vec<_> = {
            let latest = self.latest.lock().unwrap();
            c.joined_rooms().into_iter().filter(|r| !latest.contains_key(&(account, r.room_id().to_owned()))).collect()
        };
        futures_util::stream::iter(missing)
            .for_each_concurrent(4, |room| async move {
                if let Ok(page) = timeline::history(&room, None, 5).await {
                    self.note_history(account, &room.room_id().to_owned(), &page.items);
                }
            })
            .await;
    }

    pub fn note_history(&self, account: AccountId, room: &OwnedRoomId, items: &[HistoryItem]) {
        if let Some(last) = items.last() {
            let mut l = self.latest.lock().unwrap();
            let key = (account, room.clone());
            if l.get(&key).is_none_or(|old| old.ts <= last.ts) {
                l.insert(key, last.clone());
            }
        }
    }
}

/// Where `record`'s server says its client API lives now (from
/// `/.well-known/matrix/client`), or `None` if it doesn't say in time.
async fn discover(record: &AccountRecord) -> Option<String> {
    let server = record.user_id.split_once(':')?.1;
    let mut http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(6));
    if let Some(p) = &record.proxy {
        http = http.proxy(reqwest::Proxy::all(p).ok()?);
    }
    let url = format!("{}://{server}/.well-known/matrix/client", crate::rooms::scheme_for(server));
    let v: serde_json::Value = http.build().ok()?.get(url).send().await.ok()?.error_for_status().ok()?.json().await.ok()?;
    let base = v.get("m.homeserver")?.get("base_url")?.as_str()?.trim_end_matches('/');
    base.starts_with("http").then(|| base.to_owned())
}
