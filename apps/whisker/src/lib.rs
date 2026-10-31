//! Whisker GUI. The Slint UI runs on the main thread; all Matrix work runs
//! on a tokio runtime and pushes results back with `invoke_from_event_loop`.
#![recursion_limit = "256"]

mod agent_form;
mod format;
mod settings;
mod ui;

use anyhow::Result;
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::{MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedMxcUri, OwnedRoomId, OwnedUserId};
use settings::{AUTO_LOCK_MINUTES, AppSettings, ENCRYPTION_SUMMARY, Theme};
use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, VecModel, Weak};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use ui::{MsgRow, PixelCache, Pixels, initial_hue, s};
use whisker_agent::{Detected, Llm, agents_in, save_agents};
use whisker_core::timeline::{self, HistoryItem, MediaKind, UNDECRYPTABLE};
use whisker_core::{AccountId, AccountKind, AccountManager, AccountSummary, ClientEvent, LoginRequest, ModAction, VaultId};

slint::include_modules!();

const KEEP_PER_ROOM: usize = 1000;
const PAGE: u32 = 50;
const AVATAR_PX: u32 = 96;

type RoomKey = (AccountId, OwnedRoomId);

#[derive(Default)]
struct State {
    account: Option<AccountId>,
    room: Option<OwnedRoomId>,
    tag_filter: Option<String>,
    room_search: String,
    /// New logins go here: the vault unlocked or created most recently.
    active_vault: Option<VaultId>,
    messages: HashMap<RoomKey, Vec<HistoryItem>>,
    /// Pagination token for older history, per room (None: reached the start).
    older: HashMap<RoomKey, Option<String>>,
    /// Running agents, by the account they speak as.
    agents: HashMap<AccountId, JoinHandle<()>>,
    last_activity: Option<Instant>,
    refresh_pending: bool,
    synced: HashSet<AccountId>,
    detected: HashMap<AccountId, Detected>,
    reply_to: Option<OwnedEventId>,
    editing: Option<OwnedEventId>,
    /// Decoded image previews, by event.
    pictures: HashMap<OwnedEventId, Option<Pixels>>,
    /// Rooms marked read locally: the server's unread count at that moment.
    /// The badge stays hidden until the server reports a different count.
    cleared: HashMap<RoomKey, u64>,
    /// The verification on screen: (account, other user, flow id).
    verifying: Option<(AccountId, OwnedUserId, String)>,
    /// Public room directory: (server, query, next page token, results).
    explore: (String, String, Option<String>, Vec<timeline::PublicRoom>),
}

#[derive(Clone)]
struct Ctl {
    rt: tokio::runtime::Handle,
    mgr: AccountManager,
    ui: Weak<AppWindow>,
    st: Arc<Mutex<State>>,
    settings: Arc<Mutex<AppSettings>>,
    /// Decoded avatar thumbnails (None: failed). Memory only.
    avatars: PixelCache,
    loading: Arc<Mutex<HashSet<String>>>,
}

fn local(ts: MilliSecondsSinceUnixEpoch) -> Option<chrono::DateTime<chrono::Local>> {
    let ms: i64 = ts.get().into();
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.with_timezone(&chrono::Local))
}

pub(crate) fn fmt_time(ts: MilliSecondsSinceUnixEpoch) -> String {
    local(ts).map(|t| t.format("%H:%M").to_string()).unwrap_or_default()
}

/// "14:05" today, "Mon" this week, "12 Oct" before that.
fn fmt_when(ts: MilliSecondsSinceUnixEpoch) -> String {
    let Some(t) = local(ts) else { return String::new() };
    match (chrono::Local::now().date_naive() - t.date_naive()).num_days() {
        0 => t.format("%H:%M").to_string(),
        1..=6 => t.format("%a").to_string(),
        _ => t.format("%-d %b").to_string(),
    }
}

/// Divider text for a day: "Today", "Yesterday", "Monday 12 October".
fn day_label(ts: MilliSecondsSinceUnixEpoch) -> String {
    let Some(t) = local(ts) else { return String::new() };
    match (chrono::Local::now().date_naive() - t.date_naive()).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ => t.format("%A %-d %B %Y").to_string(),
    }
}

fn split_tags(t: &str) -> Vec<String> {
    t.split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned).collect()
}

fn decode(bytes: &[u8], max_w: u32, max_h: u32, square: bool) -> Option<Pixels> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = if square {
        img.resize_to_fill(max_w, max_h, image::imageops::FilterType::Triangle)
    } else if img.width() > max_w || img.height() > max_h {
        img.resize(max_w, max_h, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let img = img.to_rgba8();
    Some(Pixels::clone_from_slice(img.as_raw(), img.width(), img.height()))
}

/// Opens a web link in the system browser. Only http(s) and mailto.
fn open_link(url: &str) {
    if !(url.starts_with("https://") || url.starts_with("http://") || url.starts_with("mailto:")) {
        return;
    }
    #[cfg(target_os = "linux")]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("explorer").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(url).spawn();
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    let r: std::io::Result<()> = Ok(());
    if let Err(e) = r {
        tracing::warn!("couldn't open link: {e}");
    }
}

struct AccountRow {
    a: AccountSummary,
    unread: u64,
}
struct RoomRowData {
    pinned: bool,
    id: String,
    name: String,
    invite: bool,
    avatar: Option<OwnedMxcUri>,
    preview: String,
    time: String,
    unread: u64,
    encrypted: bool,
}

impl Ctl {
    fn ui(&self, f: impl FnOnce(&AppWindow) + Send + 'static) {
        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(u) = ui.upgrade() {
                f(&u);
            }
        });
    }

    fn spawn(&self, f: impl Future<Output = ()> + Send + 'static) {
        self.rt.spawn(f);
    }

    fn status(&self, msg: impl Into<String>) {
        let msg = msg.into();
        self.ui(move |u| u.set_status(s(msg)));
    }

    fn settings_msg(&self, msg: impl Into<String>) {
        let msg = msg.into();
        self.ui(move |u| u.set_settings_message(s(msg)));
    }

    fn touch(&self) {
        self.st.lock().unwrap().last_activity = Some(Instant::now());
    }

    fn current(&self) -> Option<RoomKey> {
        let st = self.st.lock().unwrap();
        Some((st.account?, st.room.clone()?))
    }

    async fn current_room(&self) -> Option<(RoomKey, matrix_sdk::Room)> {
        let key = self.current()?;
        let room = self.mgr.client(key.0).await?.get_room(&key.1)?;
        Some((key, room))
    }

    // ---- images ----

    /// Fetches avatars not cached yet, then redraws what shows them.
    fn fetch_avatars(&self, account: AccountId, mxcs: impl IntoIterator<Item = OwnedMxcUri>) {
        let wanted: Vec<OwnedMxcUri> = {
            let cache = self.avatars.lock().unwrap();
            let mut loading = self.loading.lock().unwrap();
            mxcs.into_iter().filter(|m| !cache.contains_key(m) && loading.insert(m.to_string())).collect()
        };
        if wanted.is_empty() {
            return;
        }
        let c = self.clone();
        self.spawn(async move {
            let Some(client) = c.mgr.client(account).await else { return };
            for m in wanted {
                let px = timeline::thumbnail(&client, &m, AVATAR_PX).await.ok().and_then(|b| decode(&b, AVATAR_PX, AVATAR_PX, true));
                c.avatars.lock().unwrap().insert(m.clone(), px);
                c.loading.lock().unwrap().remove(m.as_str());
            }
            c.refresh_accounts_only().await;
            c.refresh_rooms().await;
            c.show_messages().await;
            c.show_explore().await;
        });
    }

    /// Fetches image previews for messages in the open room.
    fn fetch_pictures(&self, key: &RoomKey, items: &[HistoryItem]) {
        let wanted: Vec<(OwnedEventId, MediaSource, bool)> = {
            let st = self.st.lock().unwrap();
            let mut loading = self.loading.lock().unwrap();
            items
                .iter()
                .filter_map(|m| {
                    let media = m.media.as_ref().filter(|x| x.kind == MediaKind::Image)?;
                    if st.pictures.contains_key(&m.event_id) || !loading.insert(m.event_id.to_string()) {
                        return None;
                    }
                    let (src, thumb) = match &media.thumbnail {
                        Some(t) => (t.clone(), false),
                        None => (media.source.clone(), true),
                    };
                    Some((m.event_id.clone(), src, thumb))
                })
                .collect()
        };
        if wanted.is_empty() {
            return;
        }
        let (c, account, room) = (self.clone(), key.0, key.1.clone());
        self.spawn(async move {
            let Some(client) = c.mgr.client(account).await else { return };
            for (id, src, thumb) in wanted {
                let bytes = timeline::download(&client, &src, thumb.then_some(640)).await.ok();
                let px = bytes.and_then(|b| decode(&b, 720, 600, false));
                c.st.lock().unwrap().pictures.insert(id.clone(), px);
                c.loading.lock().unwrap().remove(id.as_str());
                c.update_message(&(account, room.clone()), &id);
            }
        });
    }

    // ---- vaults ----

    async fn unlock(&self, pass: String) {
        self.ui(|u| {
            u.set_busy(true);
            u.set_error(s(""));
        });
        let res = if self.mgr.is_empty() { self.mgr.create_vault(&pass).await.map(|v| vec![v]) } else { self.mgr.unlock(&pass).await };
        match res {
            Ok(v) if v.is_empty() => self.ui(|u| {
                u.set_busy(false);
                u.set_error(s("No vault opens with that passphrase."));
            }),
            Ok(vaults) => {
                self.ui(|u| {
                    u.set_busy(false);
                    u.set_first_run(false);
                    u.set_screen(1);
                });
                self.after_unlock(vaults).await;
            }
            Err(e) => {
                let m = format!("{e:#}");
                self.ui(move |u| {
                    u.set_busy(false);
                    u.set_error(s(m));
                });
            }
        }
    }

    async fn after_unlock(&self, vaults: Vec<VaultId>) {
        let mut handles = Vec::new();
        for v in &vaults {
            handles.extend(whisker_agent::start_vault(&self.mgr, v).await);
        }
        {
            let mut st = self.st.lock().unwrap();
            for (account, h) in handles {
                if let Some(old) = st.agents.insert(account, h) {
                    old.abort();
                }
            }
            st.last_activity = Some(Instant::now());
            if let Some(v) = vaults.last() {
                st.active_vault = Some(v.clone());
            }
        }
        self.refresh_accounts().await;
    }

    async fn unlock_more(&self, pass: String) {
        match self.mgr.unlock(&pass).await {
            Ok(v) if v.is_empty() => self.settings_msg("No locked vault opens with that passphrase."),
            Ok(v) => {
                self.after_unlock(v).await;
                self.settings_msg("Hidden vault unlocked. New accounts go into it until you lock.");
            }
            Err(e) => self.settings_msg(format!("{e:#}")),
        }
    }

    async fn create_hidden(&self, pass: String) {
        if pass.len() < 8 {
            return self.settings_msg("Use at least 8 characters.");
        }
        // A passphrase that already opens a vault would make the two indistinguishable.
        if let Ok(v) = self.mgr.unlock(&pass).await
            && !v.is_empty()
        {
            self.after_unlock(v).await;
            return self.settings_msg("That passphrase already opens a vault (now unlocked). Pick another.");
        }
        match self.mgr.create_vault(&pass).await {
            Ok(v) => {
                self.st.lock().unwrap().active_vault = Some(v);
                self.settings_msg("Hidden vault created. Accounts you add now go into it.");
            }
            Err(e) => self.settings_msg(format!("{e:#}")),
        }
    }

    async fn lock(&self) {
        let agents = std::mem::take(&mut self.st.lock().unwrap().agents);
        for a in agents.into_values() {
            a.abort();
        }
        self.mgr.lock_all().await;
        *self.st.lock().unwrap() = State::default();
        self.avatars.lock().unwrap().clear();
        self.ui(|u| {
            ui::with(|m| m.clear());
            u.set_screen(0);
            u.set_current_account(s(""));
            u.set_current_room(s(""));
            u.set_current_room_name(s(""));
            u.set_current_account_name(s(""));
            u.set_status(s(""));
            u.set_draft(s(""));
            u.set_reply_to(s(""));
            u.set_editing(s(""));
            u.set_pane(1);
        });
    }

    // ---- accounts and rooms ----

    async fn refresh_accounts(&self) {
        self.refresh_accounts_only().await;
        self.refresh_rooms().await;
    }

    async fn refresh_accounts_only(&self) {
        let filter = self.st.lock().unwrap().tag_filter.clone();
        let list = self.mgr.accounts_tagged(filter.as_deref()).await;
        let tags: Vec<SharedString> = self.mgr.all_tags().await.iter().map(s).collect();

        let current = {
            let mut st = self.st.lock().unwrap();
            if st.account.is_none_or(|a| !list.iter().any(|x| x.id == a)) {
                st.account = list.first().map(|a| a.id);
                st.room = None;
            }
            st.account
        };
        let mut rows = Vec::new();
        for a in &list {
            let unread = match self.mgr.client(a.id).await {
                Some(c) => c.joined_rooms().iter().map(|r| r.unread_notification_counts().notification_count).sum(),
                None => 0,
            };
            if let Some(m) = &a.avatar {
                self.fetch_avatars(a.id, [m.clone()]);
            }
            rows.push(AccountRow { a: a.clone(), unread });
        }
        let sel = current.and_then(|c| list.iter().find(|a| a.id == c)).cloned();
        let px = self.avatars.clone();
        self.ui(move |u| {
            ui::with(|m| {
                let items: Vec<AccountItem> = rows
                    .iter()
                    .map(|r| {
                        let (initial, hue) = initial_hue(&r.a.display_name);
                        AccountItem {
                            id: s(r.a.id.to_string()),
                            name: s(&r.a.display_name),
                            user_id: s(&r.a.user_id),
                            tags: s(r.a.tags.iter().cloned().collect::<Vec<_>>().join(", ")),
                            is_agent: r.a.kind == AccountKind::Agent,
                            initial,
                            hue,
                            avatar: m.avatar(&r.a.avatar, &px),
                            unread: r.unread.min(999) as i32,
                        }
                    })
                    .collect();
                ui::sync_rows(&m.accounts, items, |a, b| a.id == b.id);
            });
            u.set_tags(ModelRc::new(VecModel::from(tags)));
            let (id, name, user) = match &sel {
                Some(a) => (a.id.to_string(), a.display_name.clone(), a.user_id.clone()),
                None => Default::default(),
            };
            u.set_current_account(s(id));
            u.set_current_account_name(s(name));
            u.set_current_account_user(s(user));
        });
    }

    /// Refreshes the room list soon, at most a few times a second.
    fn schedule_rooms(&self) {
        {
            let mut st = self.st.lock().unwrap();
            if st.refresh_pending {
                return;
            }
            st.refresh_pending = true;
        }
        let c = self.clone();
        self.spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            c.st.lock().unwrap().refresh_pending = false;
            c.refresh_rooms().await;
        });
    }

    async fn refresh_rooms(&self) {
        let (account, room, search, synced) = {
            let st = self.st.lock().unwrap();
            (st.account, st.room.clone(), st.room_search.to_lowercase(), st.account.is_some_and(|a| st.synced.contains(&a)))
        };
        let Some(account) = account else {
            self.ui(|u| {
                ui::with(|m| m.rooms.set_vec(Vec::new()));
                u.set_loading_rooms(false);
            });
            return;
        };
        let rooms = self.mgr.rooms(account).await;
        self.fetch_avatars(account, rooms.iter().filter_map(|r| r.avatar.clone()));

        let current = room.as_ref().and_then(|id| rooms.iter().find(|r| &r.room_id == id)).cloned();
        let cleared = {
            let mut st = self.st.lock().unwrap();
            // Forget a local "read" once the server's count moves on.
            st.cleared.retain(|(a, r), n| *a != account || rooms.iter().any(|x| &x.room_id == r && x.unread == *n));
            st.cleared.clone()
        };
        let rows: Vec<RoomRowData> = rooms
            .iter()
            .filter(|r| search.is_empty() || r.name.to_lowercase().contains(&search))
            .map(|r| {
                let preview = r
                    .latest
                    .as_ref()
                    .map(|l| {
                        if l.body == UNDECRYPTABLE {
                            "Encrypted message".to_owned()
                        } else {
                            format!("{}: {}", l.sender_label(), l.body.lines().find(|x| !x.trim().is_empty()).unwrap_or_default())
                        }
                    })
                    .unwrap_or_else(|| if r.encrypted { "Encrypted room".into() } else { String::new() });
                let read = room.as_ref() == Some(&r.room_id) || cleared.contains_key(&(account, r.room_id.clone()));
                RoomRowData {
                    pinned: r.pinned,
                    id: r.room_id.to_string(),
                    name: r.name.clone(),
                    invite: r.invite,
                    avatar: r.avatar.clone(),
                    preview,
                    time: r.latest.as_ref().map(|l| fmt_when(l.ts)).unwrap_or_default(),
                    unread: if read { 0 } else { r.unread },
                    encrypted: r.encrypted,
                }
            })
            .collect();
        let (room_id, room_name, topic, invite) = match current {
            Some(r) => (r.room_id.to_string(), r.name, r.topic.unwrap_or_default(), r.invite),
            None => Default::default(),
        };
        let px = self.avatars.clone();
        self.ui(move |u| {
            ui::with(|m| {
                let items: Vec<RoomItem> = rows
                    .iter()
                    .map(|r| {
                        let (initial, hue) = initial_hue(&r.name);
                        RoomItem {
                            id: s(&r.id),
                            name: s(&r.name),
                            invite: r.invite,
                            initial,
                            hue,
                            avatar: m.avatar(&r.avatar, &px),
                            preview: s(&r.preview),
                            time: s(&r.time),
                            unread: r.unread.min(999) as i32,
                            encrypted: r.encrypted,
                            pinned: r.pinned,
                        }
                    })
                    .collect();
                ui::sync_rows(&m.rooms, items, |a, b| a.id == b.id);
            });
            u.set_loading_rooms(!synced);
            u.set_current_room(s(room_id));
            u.set_current_invite(invite);
            u.set_current_room_name(s(room_name));
            u.set_current_room_topic(s(topic.lines().next().unwrap_or_default()));
        });
    }

    // ---- messages ----

    /// Builds display rows for a room's cached messages.
    async fn rows(&self, key: &RoomKey) -> Vec<MsgRow> {
        let me: Option<OwnedUserId> = self.mgr.client(key.0).await.and_then(|c| c.user_id().map(ToOwned::to_owned));
        let st = self.st.lock().unwrap();
        let msgs = st.messages.get(key).cloned().unwrap_or_default();
        let by_id: HashMap<&OwnedEventId, &HistoryItem> = msgs.iter().map(|m| (&m.event_id, m)).collect();
        msgs.iter()
            .enumerate()
            .map(|(i, m)| {
                let prev = i.checked_sub(1).map(|p| &msgs[p]);
                let day = match prev {
                    Some(p) if local(p.ts).map(|t| t.date_naive()) == local(m.ts).map(|t| t.date_naive()) => String::new(),
                    _ => day_label(m.ts),
                };
                let first = !day.is_empty()
                    || m.reply_to.is_some()
                    || prev.is_none_or(|p| p.sender != m.sender || i64::from(m.ts.get()) - i64::from(p.ts.get()) > 5 * 60 * 1000);
                let reply = m.reply_to.as_ref().map(|r| match by_id.get(r) {
                    Some(o) => (o.sender_label().to_owned(), o.body.lines().find(|l| !l.trim().is_empty()).unwrap_or_default().to_owned()),
                    None => ("↪".to_owned(), "an earlier message".to_owned()),
                });
                MsgRow {
                    picture: st.pictures.get(&m.event_id).cloned().flatten(),
                    mine: me.as_ref() == Some(&m.sender),
                    first,
                    day,
                    reply,
                    item: m.clone(),
                }
            })
            .collect()
    }

    /// Redraws the whole open room (on switching rooms or loading history).
    async fn show_messages(&self) {
        let Some(key) = self.current() else {
            self.ui(|_| {
                ui::with(|m| m.messages.set_vec(Vec::new()));
            });
            return;
        };
        let rows = self.rows(&key).await;
        self.fetch_avatars(key.0, rows.iter().filter_map(|r| r.item.sender_avatar.clone()));
        let items: Vec<HistoryItem> = rows.iter().map(|r| r.item.clone()).collect();
        self.fetch_pictures(&key, &items);
        let (px, older) = (self.avatars.clone(), self.st.lock().unwrap().older.get(&key).cloned().flatten().is_some());
        self.ui(move |u| {
            ui::with(|m| {
                let items: Vec<MessageItem> = rows.iter().map(|r| m.message(r, &px)).collect();
                ui::sync_rows(&m.messages, items, |a, b| a.id == b.id);
            });
            u.set_has_older(older);
        });
    }

    /// Appends the newest message of the open room without redrawing the rest.
    async fn append_last(&self, key: &RoomKey) {
        let rows = self.rows(key).await;
        let Some(last) = rows.last().cloned() else { return };
        self.fetch_avatars(key.0, last.item.sender_avatar.clone());
        self.fetch_pictures(key, std::slice::from_ref(&last.item));
        let px = self.avatars.clone();
        self.ui(move |_| {
            ui::with(|m| {
                let item = m.message(&last, &px);
                match ui::index_of(&m.messages, &item.id) {
                    Some(i) => m.messages.set_row_data(i, item),
                    None => m.messages.push(item),
                }
            });
        });
    }

    /// Redraws one message of `key` if that room is open.
    fn update_message(&self, key: &RoomKey, id: &OwnedEventId) {
        let c = self.clone();
        let (key, id) = (key.clone(), id.clone());
        self.spawn(async move {
            if c.current().as_ref() != Some(&key) {
                return;
            }
            let rows = c.rows(&key).await;
            let Some(row) = rows.into_iter().find(|r| r.item.event_id == id) else { return };
            let px = c.avatars.clone();
            c.ui(move |_| {
                ui::with(|m| {
                    if let Some(i) = ui::index_of(&m.messages, row.item.event_id.as_str()) {
                        let item = m.message(&row, &px);
                        m.messages.set_row_data(i, item);
                    }
                });
            });
        });
    }

    fn merge(&self, key: &RoomKey, items: Vec<HistoryItem>) {
        let mut st = self.st.lock().unwrap();
        let list = st.messages.entry(key.clone()).or_default();
        let known: HashSet<_> = list.iter().map(|m| m.event_id.clone()).collect();
        list.extend(items.into_iter().filter(|m| !known.contains(&m.event_id)));
        list.sort_by_key(|m| m.ts);
        if list.len() > KEEP_PER_ROOM {
            list.drain(..list.len() - KEEP_PER_ROOM);
        }
    }

    async fn select_account(&self, id: String) {
        let Ok(id) = id.parse::<AccountId>() else { return };
        {
            let mut st = self.st.lock().unwrap();
            if st.account == Some(id) {
                return;
            }
            st.account = Some(id);
            st.room = None;
            st.reply_to = None;
            st.editing = None;
            st.explore = Default::default();
        }
        self.ui(|u| {
            ui::with(|m| m.messages.set_vec(Vec::new()));
            u.set_reply_to(s(""));
            u.set_editing(s(""));
            u.set_public_rooms(ModelRc::default());
            u.set_exploring(false);
        });
        self.refresh_accounts().await;
    }

    fn cycle_tag(&self, current: String) {
        let c = self.clone();
        self.spawn(async move {
            let tags: Vec<String> = c.mgr.all_tags().await.into_iter().collect();
            let next = if current.is_empty() {
                tags.first().cloned()
            } else {
                tags.iter().position(|t| *t == current).and_then(|i| tags.get(i + 1).cloned())
            };
            c.st.lock().unwrap().tag_filter = next.clone();
            let label = next.unwrap_or_default();
            c.ui(move |u| u.set_tag_filter(s(label)));
            c.refresh_accounts().await;
        });
    }

    async fn search_rooms(&self, q: String) {
        self.st.lock().unwrap().room_search = q;
        self.refresh_rooms().await;
    }

    async fn select_room(&self, id: String) {
        let Ok(room_id) = OwnedRoomId::try_from(id) else { return };
        let Some(account) = self.st.lock().unwrap().account else { return };
        let Some(client) = self.mgr.client(account).await else { return };
        let Some(room) = client.get_room(&room_id) else { return };
        let key = (account, room_id.clone());
        // An invitation opens as a card with Accept / Decline.
        if room.state() == matrix_sdk::RoomState::Invited {
            {
                let mut st = self.st.lock().unwrap();
                st.room = Some(room_id.clone());
                st.reply_to = None;
                st.editing = None;
            }
            let from = match room.invite_details().await {
                Ok(i) => i.inviter.map(|m| m.display_name().unwrap_or(m.user_id().as_str()).to_owned()).unwrap_or_else(|| i.inviter_id.to_string()),
                Err(_) => String::new(),
            };
            self.ui(move |u| {
                u.set_current_invite(true);
                u.set_invite_from(s(from));
                u.set_loading_history(false);
            });
            self.refresh_rooms().await;
            return self.show_messages().await;
        }
        self.ui(|u| u.set_current_invite(false));
        {
            let mut st = self.st.lock().unwrap();
            st.room = Some(room_id.clone());
            st.reply_to = None;
            st.editing = None;
        }
        // Can our account kick/ban/delete others' messages here?
        let can_moderate = match (room.power_levels().await, client.user_id()) {
            (Ok(pl), Some(me)) => pl.user_can_kick(me) || pl.user_can_ban(me) || pl.user_can_redact_event_of_other(me),
            _ => false,
        };
        self.ui(move |u| {
            u.set_loading_history(true);
            u.set_can_moderate(can_moderate);
            u.set_reply_to(s(""));
            u.set_editing(s(""));
            u.set_stick_bottom(true);
        });
        self.refresh_rooms().await;
        self.show_messages().await;

        let fetched = self.st.lock().unwrap().older.contains_key(&key);
        if !fetched {
            match timeline::history(&room, None, PAGE).await {
                Ok(page) => {
                    self.mgr.note_history(account, &room_id, &page.items);
                    self.merge(&key, page.items);
                    self.st.lock().unwrap().older.insert(key.clone(), page.older);
                    if self.current().as_ref() == Some(&key) {
                        self.show_messages().await;
                    }
                }
                Err(e) => self.status(format!("Couldn't load history: {e}")),
            }
        }
        self.ui(|u| u.set_loading_history(false));
        self.mark_read(&key).await;
    }

    /// Clears a room's unread badge now and tells the server (privately).
    async fn mark_read(&self, key: &RoomKey) {
        if let Some(n) = self.mgr.rooms(key.0).await.iter().find(|r| r.room_id == key.1).map(|r| r.unread) {
            self.st.lock().unwrap().cleared.insert(key.clone(), n);
        }
        self.refresh_rooms().await;
        if let Err(e) = self.mgr.mark_read(key.0, &key.1).await {
            tracing::debug!("read receipt failed: {e}");
        }
    }

    async fn account_menu(&self, action: String, id: String) {
        let Ok(aid) = id.parse::<AccountId>() else { return };
        match action.as_str() {
            // "move:N" comes from dragging an account N places.
            a if a == "up" || a == "down" || a.starts_with("move:") => {
                let delta = match a {
                    "up" => -1,
                    "down" => 1,
                    _ => a[5..].parse().unwrap_or(0),
                };
                if let Err(e) = self.mgr.move_account(aid, delta).await {
                    self.status(format!("{e:#}"));
                }
            }
            "settings" => self.open_settings(format!("account:{id}")).await,
            "ai" => self.open_settings(format!("ai:{id}")).await,
            _ => {}
        }
    }

    async fn room_menu(&self, action: String, id: String) {
        let Ok(room_id) = OwnedRoomId::try_from(id) else { return };
        let Some(account) = self.st.lock().unwrap().account else { return };
        let Some(room) = self.mgr.client(account).await.and_then(|c| c.get_room(&room_id)) else { return };
        let key = (account, room_id.clone());
        match action.as_str() {
            "pin" | "unpin" => match room.set_is_favourite(action == "pin", None).await {
                Ok(()) => self.schedule_rooms(),
                Err(e) => self.status(format!("Couldn't change pin: {e}")),
            },
            "accept" => {
                self.status("Joining…");
                match whisker_core::rooms::accept_invite(&room).await {
                    Ok(()) => {
                        self.status("");
                        self.select_room(room_id.to_string()).await;
                    }
                    Err(e) => self.status(format!("Couldn't join: {e:#}")),
                }
            }
            "read" => self.mark_read(&key).await,
            "info" => {
                if self.current().as_ref() != Some(&key) {
                    self.select_room(room_id.to_string()).await;
                }
                self.open_room_info().await;
            }
            "copy" => {
                let link = match room.canonical_alias() {
                    Some(a) => a.to_string(),
                    None => format!("https://matrix.to/#/{room_id}"),
                };
                self.ui(move |u| u.invoke_copy_text(s(link)));
            }
            "leave" => {
                let name = timeline::room_name(&room).await;
                match room.leave().await {
                    Ok(()) => {
                        {
                            let mut st = self.st.lock().unwrap();
                            if st.room.as_ref() == Some(&room_id) {
                                st.room = None;
                            }
                        }
                        self.status(format!("Left {name}"));
                        self.show_messages().await;
                        self.refresh_rooms().await;
                    }
                    Err(e) => self.status(format!("Couldn't leave: {e}")),
                }
            }
            _ => {}
        }
    }

    async fn explore_search(&self, server: String, query: String) {
        self.st.lock().unwrap().explore = (server.trim().to_owned(), query.trim().to_owned(), None, Vec::new());
        self.explore_page().await;
    }

    async fn explore_more(&self) {
        if self.st.lock().unwrap().explore.2.is_some() {
            self.explore_page().await;
        }
    }

    async fn explore_page(&self) {
        let Some(account) = self.st.lock().unwrap().account else { return };
        let Some(client) = self.mgr.client(account).await else { return };
        let (server, query, since, _) = self.st.lock().unwrap().explore.clone();
        self.ui(|u| {
            u.set_explore_busy(true);
            u.set_explore_status(s("Searching…"));
        });
        let res = whisker_core::rooms::public_rooms(&client, &server, &query, since).await;
        let status = match res {
            Ok((rooms, next)) => {
                self.fetch_avatars(account, rooms.iter().filter_map(|r| r.avatar.clone()));
                let mut st = self.st.lock().unwrap();
                st.explore.2 = next;
                st.explore.3.extend(rooms);
                if st.explore.3.is_empty() { "No public rooms found.".to_owned() } else { String::new() }
            }
            Err(e) => format!("Couldn't load the directory: {e:#}"),
        };
        self.ui(move |u| {
            u.set_explore_busy(false);
            u.set_explore_status(s(status));
        });
        self.show_explore().await;
    }

    /// Redraws the public room list (also after avatars arrive).
    async fn show_explore(&self) {
        let Some(account) = self.st.lock().unwrap().account else { return };
        let Some(client) = self.mgr.client(account).await else { return };
        let (rooms, more) = {
            let st = self.st.lock().unwrap();
            (st.explore.3.clone(), st.explore.2.is_some())
        };
        let joined: HashSet<OwnedRoomId> = client.joined_rooms().iter().map(|r| r.room_id().to_owned()).collect();
        let px = self.avatars.clone();
        self.ui(move |u| {
            let items: Vec<PublicItem> = ui::with(|m| {
                rooms
                    .iter()
                    .map(|r| {
                        let (initial, hue) = initial_hue(&r.name);
                        PublicItem {
                            id: s(r.room_id.as_str()),
                            name: s(&r.name),
                            alias: s(r.alias.clone().unwrap_or_default()),
                            topic: s(r.topic.as_deref().unwrap_or_default().lines().next().unwrap_or_default()),
                            members: s(format!("{} members", r.members)),
                            initial,
                            hue,
                            avatar: m.avatar(&r.avatar, &px),
                            joined: joined.contains(&r.room_id),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
            u.set_public_rooms(ModelRc::new(VecModel::from(items)));
            u.set_explore_has_more(more);
        });
    }

    async fn explore_join(&self, target: String) {
        let Some(account) = self.st.lock().unwrap().account else { return };
        let Some(client) = self.mgr.client(account).await else { return };
        let joined_id = OwnedRoomId::try_from(target.trim()).ok().filter(|id| client.get_room(id).is_some_and(|r| r.state() == matrix_sdk::RoomState::Joined));
        let room_id = match joined_id {
            Some(id) => id,
            None => {
                self.ui(|u| {
                    u.set_explore_busy(true);
                    u.set_explore_status(s("Joining…"));
                });
                let r = timeline::join(&client, &target).await;
                self.ui(|u| u.set_explore_busy(false));
                match r {
                    Ok(room) => room.room_id().to_owned(),
                    Err(e) => {
                        let m = format!("Couldn't join: {e:#}");
                        return self.ui(move |u| u.set_explore_status(s(m)));
                    }
                }
            }
        };
        self.ui(|u| {
            u.set_explore_status(s(""));
            u.set_exploring(false);
            u.set_pane(2);
            u.set_stick_bottom(true);
        });
        self.select_room(room_id.to_string()).await;
    }

    async fn profile_edit(&self, action: String, id: String, value: String) {
        let Ok(aid) = id.parse::<AccountId>() else { return };
        let Some(client) = self.mgr.client(aid).await else { return };
        let r = match action.as_str() {
            "name" => whisker_core::rooms::set_display_name(&client, &value).await,
            "remove-avatar" => whisker_core::rooms::set_avatar(&client, None).await,
            "verify-self" => {
                let Some(me) = client.user_id().map(ToOwned::to_owned) else { return };
                self.ui(|u| {
                    u.set_verify_self(true);
                    u.set_verify_state(s("waiting"));
                    u.set_verify_message(s(""));
                    u.set_verify_open(true);
                });
                if let Err(e) = whisker_core::verify::start(&client, &self.mgr.event_sender(), aid, &me).await {
                    let m = format!("{e:#}");
                    self.ui(move |u| {
                        u.set_verify_state(s("cancelled"));
                        u.set_verify_message(s(m));
                    });
                }
                return;
            }
            "avatar" => {
                #[cfg(not(target_os = "android"))]
                {
                    let picked = tokio::task::spawn_blocking(|| {
                        rfd::FileDialog::new().set_title("Choose a profile picture").add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif"]).pick_file()
                    })
                    .await
                    .ok()
                    .flatten();
                    let Some(path) = picked else { return };
                    self.settings_msg("Uploading picture…");
                    whisker_core::rooms::set_avatar(&client, Some(&path)).await
                }
                #[cfg(target_os = "android")]
                Err(anyhow::anyhow!("changing your picture isn't available on Android yet"))
            }
            _ => return,
        };
        match r {
            Ok(()) => {
                self.mgr.refresh_profile(aid).await;
                self.open_settings(format!("account:{aid}")).await;
                self.settings_msg("Profile updated.");
            }
            Err(e) => self.settings_msg(format!("Couldn't update your profile: {e:#}")),
        }
    }

    async fn verify_action(&self, action: String) {
        let current = self.st.lock().unwrap().verifying.clone();
        if action == "close" {
            self.st.lock().unwrap().verifying = None;
            self.ui(|u| u.set_verify_open(false));
            // The verified badge may have changed.
            if let Some((_, user, _)) = current
                && let Some(key) = self.current()
            {
                self.open_profile(&key, user).await;
            }
            return;
        }
        let Some((account, user, flow)) = current else {
            return self.ui(|u| u.set_verify_open(false));
        };
        let Some(client) = self.mgr.client(account).await else { return };
        let tx = self.mgr.event_sender();
        let r = match action.as_str() {
            "accept" => whisker_core::verify::accept(&client, &tx, account, &user, &flow).await,
            "match" => whisker_core::verify::confirm(&client, &user, &flow, true).await,
            "mismatch" => whisker_core::verify::confirm(&client, &user, &flow, false).await,
            "cancel" => {
                let r = whisker_core::verify::cancel(&client, &user, &flow).await;
                self.st.lock().unwrap().verifying = None;
                self.ui(|u| u.set_verify_open(false));
                r
            }
            _ => Ok(()),
        };
        if let Err(e) = r {
            let m = format!("{e:#}");
            self.ui(move |u| {
                u.set_verify_state(s("cancelled"));
                u.set_verify_message(s(m));
            });
        }
    }

    async fn on_verification(&self, account: AccountId, user: OwnedUserId, flow: String, state: whisker_core::verify::VerifyState) {
        use whisker_core::verify::VerifyState as V;
        {
            let mut st = self.st.lock().unwrap();
            // One verification at a time; others time out on their own.
            if st.verifying.as_ref().is_some_and(|(a, u, f)| (*a, u, f) != (account, &user, &flow)) {
                return;
            }
            st.verifying = Some((account, user.clone(), flow.clone()));
        }
        let name = match self.mgr.client(account).await {
            Some(c) => match c.account().fetch_user_profile_of(&user).await {
                Ok(p) => p.get_static::<matrix_sdk::ruma::profile::DisplayName>().ok().flatten().unwrap_or_else(|| user.localpart().to_owned()),
                Err(_) => user.localpart().to_owned(),
            },
            None => user.localpart().to_owned(),
        };
        let is_self = self.mgr.client(account).await.and_then(|c| c.user_id().map(|u| u == user)).unwrap_or(false);
        let (tag, emojis, msg) = match state {
            V::Incoming { .. } => ("incoming", vec![], String::new()),
            V::Waiting => ("waiting", vec![], String::new()),
            V::Emojis(e) => ("emojis", e, String::new()),
            V::Confirmed => ("confirmed", vec![], String::new()),
            V::Done => ("done", vec![], String::new()),
            V::Cancelled(m) => ("cancelled", vec![], m),
        };
        if matches!(tag, "done" | "cancelled") {
            // Keep the screen, but stop tracking so new requests can come in.
            self.st.lock().unwrap().verifying = None;
        }
        self.ui(move |u| {
            let items: Vec<EmojiItem> = emojis.iter().map(|(sym, n)| EmojiItem { symbol: s(sym), name: s(n) }).collect();
            u.set_verify_emojis(ModelRc::new(VecModel::from(items)));
            u.set_verify_name(s(&name));
            u.set_verify_self(is_self);
            u.set_verify_state(s(tag));
            u.set_verify_message(s(msg));
            u.set_verify_open(true);
        });
    }

    async fn profile_requested(&self, user: String) {
        let Ok(user) = OwnedUserId::try_from(user) else { return };
        let Some(key) = self.current() else { return };
        self.open_profile(&key, user).await;
    }

    /// Shows the room info panel: description, pinned messages, settings, members.
    async fn open_room_info(&self) {
        let Some((key, room)) = self.current_room().await else { return };
        let d = whisker_core::rooms::details(&room).await;
        self.fetch_avatars(key.0, d.avatar.clone());
        let px = self.avatars.clone();
        let (id, pinned) = (key.1.to_string(), d.pinned.clone());
        self.ui(move |u| {
            let (initial, hue) = initial_hue(&d.name);
            let avatar = ui::with(|m| m.avatar(&d.avatar, &px)).unwrap_or_default();
            u.set_room_details(RoomDetails {
                id: s(&id),
                name: s(&d.name),
                topic: s(&d.topic),
                alias: s(&d.alias),
                initial,
                hue,
                avatar,
                encrypted: d.encrypted,
                join_rule: s(&d.join_rule),
                history: s(&d.history),
                members: s(format!("{} members", d.members)),
                can_edit: d.can_edit,
                can_invite: d.can_invite,
            });
            let pins: Vec<PinnedItem> = pinned.iter().map(|(w, t)| PinnedItem { who: s(w), text: s(t) }).collect();
            u.set_pinned(ModelRc::new(VecModel::from(pins)));
            u.set_members(ModelRc::default());
            u.set_members_loading(true);
            u.set_room_info_status(s(""));
            u.set_info_editing(false);
            u.set_profile_open(false);
            u.set_room_info_open(true);
            u.set_pane(2);
        });
        match whisker_core::rooms::members(&room).await {
            Ok(list) => {
                self.fetch_avatars(key.0, list.iter().take(200).filter_map(|m| m.avatar.clone()));
                let px = self.avatars.clone();
                self.ui(move |u| {
                    let items: Vec<MemberItem> = ui::with(|m| {
                        list.iter()
                            .map(|mem| {
                                let (initial, hue) = initial_hue(&mem.name);
                                MemberItem {
                                    user_id: s(mem.user_id.as_str()),
                                    name: s(&mem.name),
                                    initial,
                                    hue,
                                    avatar: m.avatar(&mem.avatar, &px),
                                    role: s(whisker_core::rooms::role_name(mem.level)),
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                    u.set_members(ModelRc::new(VecModel::from(items)));
                    u.set_members_loading(false);
                });
            }
            Err(e) => {
                let m = format!("Couldn't load members: {e}");
                self.ui(move |u| {
                    u.set_members_loading(false);
                    u.set_room_info_status(s(m));
                });
            }
        }
    }

    async fn room_invite(&self, users: String) {
        let Some((_, room)) = self.current_room().await else { return };
        let m = match whisker_core::rooms::invite(&room, &users).await {
            Ok(n) => format!("Invited {n} {}.", if n == 1 { "person" } else { "people" }),
            Err(e) => format!("{e:#}"),
        };
        self.ui(move |u| u.set_room_info_status(s(m)));
    }

    async fn room_edit(&self, name: String, topic: String) {
        let Some((_, room)) = self.current_room().await else { return };
        match whisker_core::rooms::edit(&room, &name, &topic).await {
            Ok(()) => {
                self.ui(|u| u.set_room_info_status(s("Saved.")));
                self.open_room_info().await;
                self.schedule_rooms();
            }
            Err(e) => {
                let m = format!("Couldn't save: {e}");
                self.ui(move |u| u.set_room_info_status(s(m)));
            }
        }
    }

    async fn create_room(&self, name: String, topic: String, public: bool, alias: String, encrypted: bool, invite: String) {
        let Some(account) = self.st.lock().unwrap().account else { return };
        let Some(client) = self.mgr.client(account).await else { return };
        let invite = match whisker_core::rooms::parse_users(&invite) {
            Ok(i) => i,
            Err(e) => {
                let m = format!("{e:#}");
                return self.ui(move |u| u.set_create_error(s(m)));
            }
        };
        self.ui(|u| {
            u.set_busy(true);
            u.set_create_error(s(""));
        });
        let res = whisker_core::rooms::create_room(&client, whisker_core::rooms::NewRoom { name, topic, public, alias, encrypted, invite }).await;
        self.ui(|u| u.set_busy(false));
        match res {
            Ok(room) => {
                self.ui(|u| {
                    u.set_create_open(false);
                    u.set_exploring(false);
                    u.set_pane(2);
                    u.set_stick_bottom(true);
                });
                self.select_room(room.room_id().to_string()).await;
            }
            Err(e) => {
                let m = format!("{e:#}");
                self.ui(move |u| u.set_create_error(s(m)));
            }
        }
    }

    async fn load_older(&self) {
        let Some((key, room)) = self.current_room().await else { return };
        let Some(Some(token)) = self.st.lock().unwrap().older.get(&key).cloned() else { return };
        self.ui(|u| u.set_loading_history(true));
        match timeline::history(&room, Some(token), PAGE).await {
            Ok(page) => {
                self.merge(&key, page.items);
                self.st.lock().unwrap().older.insert(key.clone(), page.older);
                self.show_messages().await;
            }
            Err(e) => self.status(format!("Couldn't load older messages: {e}")),
        }
        self.ui(|u| u.set_loading_history(false));
    }

    async fn send(&self, text: String) {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        let Some((_, room)) = self.current_room().await else { return };
        let (reply_to, editing) = {
            let mut st = self.st.lock().unwrap();
            (st.reply_to.take(), st.editing.take())
        };
        self.ui(|u| {
            u.set_reply_to(s(""));
            u.set_editing(s(""));
            u.set_status(s(""));
        });

        if let Some(target) = editing {
            if let Err(e) = timeline::edit_text(&room, &target, &text).await {
                self.status(format!("Not saved: {e:#}"));
            }
            return;
        }
        if let Some(cmd) = ModAction::parse_command(&text) {
            match cmd {
                Ok(action) => match action.apply(&room).await {
                    Ok(()) => self.status(format!("Done: {action}")),
                    Err(e) => self.status(format!("Failed: {e}")),
                },
                Err(e) => self.status(format!("{e}")),
            }
            return;
        }
        if let Err(e) = timeline::send_text(&room, &text, reply_to.as_deref()).await {
            self.status(format!("Not sent: {e:#}"));
        }
    }

    async fn attach(&self) {
        #[cfg(not(target_os = "android"))]
        {
            let Some((_, room)) = self.current_room().await else { return };
            let picked = tokio::task::spawn_blocking(|| rfd::FileDialog::new().set_title("Send files").pick_files()).await.ok().flatten();
            for path in picked.unwrap_or_default() {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.status(format!("Uploading {name}…"));
                match timeline::send_file(&room, &path).await {
                    Ok(()) => self.status(""),
                    Err(e) => self.status(format!("Couldn't send {name}: {e:#}")),
                }
            }
        }
        #[cfg(target_os = "android")]
        self.status("Uploading isn't available on Android yet.");
    }

    fn find(&self, key: &RoomKey, id: &str) -> Option<HistoryItem> {
        self.st.lock().unwrap().messages.get(key)?.iter().find(|m| m.event_id.as_str() == id).cloned()
    }

    async fn message_action(&self, action: String, id: String) {
        let Some((key, room)) = self.current_room().await else { return };
        let Some(m) = self.find(&key, &id) else { return };
        match action.as_str() {
            "reply" => {
                self.st.lock().unwrap().reply_to = Some(m.event_id.clone());
                let label = format!("{}: {}", m.sender_label(), m.body.lines().next().unwrap_or_default());
                self.ui(move |u| {
                    u.set_editing(s(""));
                    u.set_reply_label(s(label));
                    u.set_reply_to(s(&id));
                });
            }
            "quote" => {
                let quoted: String = m.body.lines().map(|l| format!("> {l}\n")).collect();
                self.ui(move |u| {
                    let draft = u.get_draft();
                    u.set_draft(s(format!("{quoted}\n{draft}")));
                });
            }
            "select" => {
                let body = m.body.clone();
                self.ui(move |u| u.set_select_text(s(body)));
            }
            "edit" => {
                self.st.lock().unwrap().editing = Some(m.event_id.clone());
                let body = m.body.clone();
                self.ui(move |u| {
                    u.set_reply_to(s(""));
                    u.set_editing(s(&id));
                    u.set_draft(s(body));
                });
            }
            "delete" => match room.redact(&m.event_id, None, None).await {
                Ok(_) => self.remove_message(&key, &m.event_id),
                Err(e) => self.status(format!("Couldn't delete: {e}")),
            },
            "kick" | "ban" => {
                let a = if action == "kick" {
                    ModAction::Kick { user: m.sender.clone(), reason: None }
                } else {
                    ModAction::Ban { user: m.sender.clone(), reason: None }
                };
                match a.apply(&room).await {
                    Ok(()) => self.status(format!("Done: {a}")),
                    Err(e) => self.status(format!("Failed: {e}")),
                }
            }
            "profile" => self.open_profile(&key, m.sender.clone()).await,
            "save" => self.save_media(&key, m).await,
            "view" => self.view_image(&key, m).await,
            _ => {}
        }
    }

    async fn save_media(&self, key: &RoomKey, m: HistoryItem) {
        #[cfg(not(target_os = "android"))]
        {
            let Some(media) = m.media else { return };
            let name = media.name.clone();
            let Some(path) = tokio::task::spawn_blocking(move || rfd::FileDialog::new().set_file_name(name).save_file()).await.ok().flatten() else {
                return;
            };
            let Some(client) = self.mgr.client(key.0).await else { return };
            self.status("Downloading…");
            let r = async {
                let bytes = timeline::download(&client, &media.source, None).await?;
                tokio::fs::write(&path, bytes).await?;
                anyhow::Ok(())
            }
            .await;
            self.status(match r {
                Ok(()) => format!("Saved to {}", path.display()),
                Err(e) => format!("Couldn't save: {e:#}"),
            });
        }
        #[cfg(target_os = "android")]
        {
            let _ = (key, m);
            self.status("Saving files isn't available on Android yet.");
        }
    }

    async fn view_image(&self, key: &RoomKey, m: HistoryItem) {
        let Some(media) = m.media else { return };
        self.ui(|u| {
            u.set_viewer_image(Image::default());
            u.set_viewer_open(true);
        });
        let Some(client) = self.mgr.client(key.0).await else { return };
        match timeline::download(&client, &media.source, None).await.ok().and_then(|b| decode(&b, 2560, 2560, false)) {
            Some(px) => self.ui(move |u| u.set_viewer_image(Image::from_rgba8(px))),
            None => {
                self.ui(|u| u.set_viewer_open(false));
                self.status("Couldn't open that image.");
            }
        }
    }

    /// Shows a user's profile card: name, avatar, role in this room and
    /// what we can do about them.
    async fn open_profile(&self, key: &RoomKey, user: OwnedUserId) {
        let Some(client) = self.mgr.client(key.0).await else { return };
        let Some(room) = client.get_room(&key.1) else { return };
        let me = client.user_id().map(ToOwned::to_owned);
        let (name0, avatar0) = timeline::member_profile(&room, &user).await;
        let name = name0.clone().unwrap_or_else(|| user.localpart().to_owned());
        let (initial, hue) = initial_hue(&name);
        let (uid, quick_name, is_me0) = (user.to_string(), name.clone(), me.as_ref() == Some(&user));
        self.ui(move |u| {
            // The card may already be open (opened instantly by the UI); keep its picture.
            let mut p = u.get_profile();
            if p.user_id != uid.as_str() {
                p = Profile { user_id: s(&uid), name: s(&quick_name), initial: initial.clone(), hue, ..Default::default() };
            }
            p.is_me = is_me0;
            u.set_profile(p);
            u.set_profile_loading(true);
            u.set_profile_open(true);
        });

        // Role and permissions in this room.
        let (role, can_kick, can_ban) = match (room.power_levels().await, &me) {
            (Ok(pl), Some(me)) => {
                let level: i64 = match pl.for_user(&user) {
                    matrix_sdk::ruma::events::room::power_levels::UserPowerLevel::Int(i) => i.into(),
                    _ => i64::MAX,
                };
                let role = match level {
                    i64::MAX => "Creator".to_owned(),
                    100.. => "Admin".to_owned(),
                    50..=99 => "Moderator".to_owned(),
                    1..=49 => format!("Power {level}"),
                    _ => String::new(),
                };
                (role, pl.user_can_kick_user(me, &user), pl.user_can_ban_user(me, &user))
            }
            _ => (String::new(), false, false),
        };
        // Server profile may be newer than this room's member info.
        let (name, avatar_mxc) = match client.account().fetch_user_profile_of(&user).await {
            Ok(p) => (
                p.get_static::<matrix_sdk::ruma::profile::DisplayName>().ok().flatten().or(name0).unwrap_or(name),
                p.get_static::<matrix_sdk::ruma::profile::AvatarUrl>().ok().flatten().or(avatar0),
            ),
            Err(_) => (name0.unwrap_or(name), avatar0),
        };
        let px = match &avatar_mxc {
            Some(m) => timeline::thumbnail(&client, m, 256).await.ok().and_then(|b| decode(&b, 256, 256, true)),
            None => None,
        };
        let has_dm = client.get_dm_room(&user).is_some();
        // 2 verified, 1 can be verified, 0 nothing to verify.
        let verified = if me.as_ref() == Some(&user) {
            match client.encryption().get_own_device().await {
                Ok(Some(d)) if d.is_cross_signed_by_owner() => 2,
                Ok(Some(_)) => 1,
                _ => 0,
            }
        } else {
            match whisker_core::verify::is_verified(&client, &user).await {
                Some(true) => 2,
                Some(false) => 1,
                None => 0,
            }
        };
        let (initial, hue) = initial_hue(&name);
        let is_me = me.as_ref() == Some(&user);
        let user_id = user.to_string();
        self.ui(move |u| {
            if u.get_profile().user_id != user_id.as_str() {
                return;
            }
            u.set_profile(Profile {
                user_id: s(&user_id),
                name: s(name),
                initial,
                hue,
                avatar: px.map(Image::from_rgba8).unwrap_or_else(|| u.get_profile().avatar),
                role: s(role),
                is_me,
                can_kick,
                can_ban,
                has_dm,
                verified,
            });
            u.set_profile_loading(false);
        });
    }

    async fn profile_action(&self, action: String, user: String) {
        let Ok(user) = OwnedUserId::try_from(user) else { return };
        let Some((key, room)) = self.current_room().await else { return };
        let Some(client) = self.mgr.client(key.0).await else { return };
        let close = |c: &Self| c.ui(|u| u.set_profile_open(false));
        match action.as_str() {
            "message" => {
                close(self);
                let dm = match client.get_dm_room(&user) {
                    Some(r) => Ok(r),
                    None => {
                        self.status(format!("Starting a chat with {user}…"));
                        client.create_dm(&user).await
                    }
                };
                match dm {
                    Ok(r) => {
                        self.status("");
                        let id = r.room_id().to_string();
                        self.ui(|u| u.set_pane(2));
                        self.select_room(id).await;
                    }
                    Err(e) => self.status(format!("Couldn't start a chat: {e}")),
                }
            }
            "mention" => {
                close(self);
                let name = timeline::member_profile(&room, &user).await.0.unwrap_or_else(|| user.localpart().to_owned());
                let pill = format!("[{name}](https://matrix.to/#/{user}) ");
                self.ui(move |u| {
                    let d = u.get_draft();
                    u.set_draft(s(format!("{d}{pill}")));
                });
            }
            "kick" | "ban" => {
                close(self);
                let a = if action == "kick" {
                    ModAction::Kick { user, reason: None }
                } else {
                    ModAction::Ban { user, reason: None }
                };
                self.status(match a.apply(&room).await {
                    Ok(()) => format!("Done: {a}"),
                    Err(e) => format!("Failed: {e}"),
                });
            }
            "verify" => {
                let name = timeline::member_profile(&room, &user).await.0.unwrap_or_else(|| user.localpart().to_owned());
                let is_self = client.user_id() == Some(&*user);
                self.ui(move |u| {
                    u.set_verify_name(s(&name));
                    u.set_verify_self(is_self);
                    u.set_verify_state(s("waiting"));
                    u.set_verify_message(s(""));
                    u.set_verify_open(true);
                });
                if let Err(e) = whisker_core::verify::start(&client, &self.mgr.event_sender(), key.0, &user).await {
                    let m = format!("{e:#}");
                    self.ui(move |u| {
                        u.set_verify_state(s("cancelled"));
                        u.set_verify_message(s(m));
                    });
                }
            }
            "ignore" => {
                close(self);
                self.status(match client.account().ignore_user(&user).await {
                    Ok(()) => format!("Ignoring {user}. Their messages won't be shown."),
                    Err(e) => format!("Couldn't ignore: {e}"),
                });
            }
            _ => {}
        }
    }

    fn set_rich_text(&self, on: bool) {
        let mut st = self.settings.lock().unwrap();
        st.rich_text = on;
        let _ = st.save(self.mgr.data_dir());
        drop(st);
        ui::RICH.set(on);
        if let Some(u) = self.ui.upgrade() {
            u.global::<Prefs>().set_rich_text(on);
        }
        let c = self.clone();
        self.spawn(async move { c.show_messages().await });
    }

    fn remove_message(&self, key: &RoomKey, id: &OwnedEventId) {
        if let Some(list) = self.st.lock().unwrap().messages.get_mut(key) {
            list.retain(|m| &m.event_id != id);
        }
        if self.current().as_ref() == Some(key) {
            let c = self.clone();
            self.spawn(async move { c.show_messages().await });
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn login(&self, hs: String, user: String, pass: String, tags: String, proxy: String, agent: bool, register: bool, token: String) {
        let Some(vault) = self.st.lock().unwrap().active_vault.clone() else { return };
        self.ui(|u| {
            u.set_busy(true);
            u.set_login_error(s(""));
        });
        let res = self
            .mgr
            .login(LoginRequest {
                vault: &vault,
                register,
                token: Some(token.trim()).filter(|t| !t.is_empty()),
                homeserver: hs.trim(),
                username: user.trim(),
                password: &pass,
                kind: if agent { AccountKind::Agent } else { AccountKind::Human },
                tags: split_tags(&tags).into_iter().collect::<BTreeSet<_>>(),
                proxy: Some(proxy.trim().to_owned()).filter(|p| !p.is_empty()),
            })
            .await;
        match res {
            Ok(id) => {
                self.ui(|u| {
                    u.set_busy(false);
                    u.set_show_login(false);
                });
                if agent {
                    self.open_settings(format!("ai:{id}")).await;
                } else {
                    self.st.lock().unwrap().account = Some(id);
                    self.refresh_accounts().await;
                }
            }
            Err(e) => {
                let m = format!("{e:#}");
                // Sentence case for the dialog.
                let m = m.chars().next().map(|c| c.to_uppercase().collect::<String>() + &m[c.len_utf8()..]).unwrap_or(m);
                self.ui(move |u| {
                    u.set_busy(false);
                    u.set_login_error(s(m));
                });
            }
        }
    }

    // ---- settings ----

    fn set_theme(&self, i: i32) {
        let mut st = self.settings.lock().unwrap();
        st.theme = Theme::from_index(i);
        let _ = st.save(self.mgr.data_dir());
    }

    fn set_auto_lock(&self, i: i32) {
        let mut st = self.settings.lock().unwrap();
        st.auto_lock_minutes = AUTO_LOCK_MINUTES.get(i as usize).copied().unwrap_or(0);
        let _ = st.save(self.mgr.data_dir());
    }

    /// Shows settings at page `id` ("general", "account:<id>", "ai:<id>").
    async fn open_settings(&self, id: String) {
        let id = if id.is_empty() { "general".to_owned() } else { id };
        let accounts = self.mgr.accounts().await;
        // (id, title, subtitle, section, robot, avatar): plain data, Slint structs are built on the UI thread.
        type NavData = (String, String, String, &'static str, bool, Option<OwnedMxcUri>);
        let mut nav: Vec<NavData> = vec![("general".into(), "General".into(), String::new(), "", false, None)];
        for (kind, prefix, section) in [(AccountKind::Human, "account", "ACCOUNTS"), (AccountKind::Agent, "ai", "AI")] {
            for a in accounts.iter().filter(|a| a.kind == kind) {
                nav.push((format!("{prefix}:{}", a.id), a.display_name.clone(), a.user_id.clone(), section, kind == AccountKind::Agent, a.avatar.clone()));
            }
        }

        // Plain data for the account page; the Slint struct is built on the UI thread.
        let mut account_data: Option<(AccountSummary, Option<String>, Option<String>)> = None;
        let mut agent = AgentForm::default();
        let mut models: Vec<SharedString> = Vec::new();
        let mut detect = String::new();
        if let Some(rest) = id.strip_prefix("account:")
            && let Ok(aid) = rest.parse::<AccountId>()
            && let Some(a) = accounts.iter().find(|a| a.id == aid)
        {
            let (label, proxy) = self.mgr.details(aid).await.unwrap_or_default();
            if let Some(m) = &a.avatar {
                self.fetch_avatars(aid, [m.clone()]);
            }
            account_data = Some((a.clone(), label, proxy));
        }
        if let Some(rest) = id.strip_prefix("ai:")
            && let Ok(aid) = rest.parse::<AccountId>()
            && let Some(a) = accounts.iter().find(|a| a.id == aid)
        {
            let cfgs = agents_in(&self.mgr, &a.vault).await;
            let cfg = cfgs.iter().find(|c| c.account == aid);
            agent = agent_form::to_form(aid, &a.display_name, cfg);
            if let Some(d) = self.st.lock().unwrap().detected.get(&aid) {
                models = d.models.iter().map(s).collect();
                detect = format!("✓ {} · {} models", d.service, d.models.len());
            } else if let Some(c) = cfg
                && !c.api.service.is_empty()
            {
                detect = format!("✓ {}", c.api.service);
            }
        }
        let px = self.avatars.clone();
        self.ui(move |u| {
            let nav: Vec<NavItem> = ui::with(|m| {
                nav.iter()
                    .map(|(id, title, sub, section, robot, mxc)| {
                        let (initial, hue) = initial_hue(title);
                        NavItem {
                            id: s(id),
                            title: s(title),
                            subtitle: s(sub),
                            section: s(section),
                            initial,
                            hue,
                            avatar: m.avatar(mxc, &px),
                            robot: *robot,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
            u.set_nav(ModelRc::new(VecModel::from(nav)));
            u.set_nav_selected(s(&id));
            let account_form = match &account_data {
                Some((a, label, proxy)) => {
                    let (initial, hue) = initial_hue(&a.display_name);
                    AccountForm {
                        id: s(a.id.to_string()),
                        user_id: s(&a.user_id),
                        display_name: s(a.profile_name.clone().unwrap_or_default()),
                        initial,
                        hue,
                        avatar: ui::with(|m| m.avatar(&a.avatar, &px)).unwrap_or_default(),
                        is_agent: a.kind == AccountKind::Agent,
                        label: s(label.clone().unwrap_or_default()),
                        tags: s(a.tags.iter().cloned().collect::<Vec<_>>().join(", ")),
                        proxy: s(proxy.clone().unwrap_or_default()),
                    }
                }
                None => AccountForm::default(),
            };
            u.set_account_form(account_form);
            u.set_agent_form(agent);
            u.set_models(ModelRc::new(VecModel::from(models)));
            u.set_detect_status(s(detect));
            u.set_test_status(s(""));
            u.set_settings_message(s(""));
            u.set_screen(2);
        });
    }

    /// (account id, nickname, tags, proxy) from the account page.
    async fn save_account(&self, (id, label, tags, proxy): (String, String, String, String)) {
        let Ok(id) = id.parse::<AccountId>() else { return };
        let r = async {
            self.mgr.set_label(id, Some(label)).await?;
            self.mgr.set_tags(id, split_tags(&tags)).await?;
            self.mgr.set_proxy(id, Some(proxy)).await
        }
        .await;
        self.settings_msg(match r {
            Ok(()) => "Saved.".to_owned(),
            Err(e) => format!("{e:#}"),
        });
    }

    fn settings_account(&self) -> Option<AccountId> {
        let ui = self.ui.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = slint::invoke_from_event_loop(move || {
            let _ = tx.send(ui.upgrade().map(|u| u.get_nav_selected().to_string()));
        });
        let sel = rx.recv_timeout(Duration::from_secs(2)).ok().flatten()?;
        sel.split_once(':').and_then(|(_, id)| id.parse().ok())
    }

    async fn recover(&self, key: String) {
        let Some(id) = self.settings_account() else { return };
        self.settings_msg("Restoring keys…");
        self.settings_msg(match self.mgr.recover(id, &key).await {
            Ok(()) => "Keys restored. Older encrypted messages will show when you reopen a room.".to_owned(),
            Err(e) => format!("Couldn't restore keys: {e:#}"),
        });
        let mut st = self.st.lock().unwrap();
        st.messages.retain(|(a, _), _| *a != id);
        st.older.retain(|(a, _), _| *a != id);
    }

    async fn logout(&self, id: String) {
        let Ok(a) = id.parse::<AccountId>() else { return };
        if let Some(h) = self.st.lock().unwrap().agents.remove(&a) {
            h.abort();
        }
        match self.mgr.logout(a).await {
            Ok(()) => {
                {
                    let mut st = self.st.lock().unwrap();
                    if st.account == Some(a) {
                        st.account = None;
                        st.room = None;
                    }
                }
                self.ui(|u| u.set_screen(1));
                self.refresh_accounts().await;
            }
            Err(e) => self.settings_msg(format!("{e:#}")),
        }
    }

    async fn vault_of(&self, id: AccountId) -> Option<VaultId> {
        self.mgr.accounts().await.into_iter().find(|a| a.id == id).map(|a| a.vault)
    }

    async fn detect_api(&self, account: AccountId, url: String, key: String) {
        self.ui(|u| u.set_detect_status(s("Connecting…")));
        // An empty key field means "use the saved one".
        let key = if key.trim().is_empty() {
            match self.vault_of(account).await {
                Some(v) => agents_in(&self.mgr, &v).await.into_iter().find(|c| c.account == account).and_then(|c| c.api.api_key),
                None => None,
            }
        } else {
            Some(key)
        };
        match whisker_agent::detect(&url, key.as_deref()).await {
            Ok(d) => {
                let status = format!(
                    "✓ {} ({}) · {} models",
                    d.service,
                    if d.format == whisker_agent::ApiFormat::AnthropicMessages { "Anthropic-style" } else { "OpenAI-style" },
                    d.models.len()
                );
                let models: Vec<SharedString> = d.models.iter().map(s).collect();
                self.st.lock().unwrap().detected.insert(account, d);
                self.ui(move |u| {
                    u.set_models(ModelRc::new(VecModel::from(models)));
                    u.set_detect_status(s(status));
                });
            }
            Err(e) => {
                let m = format!("✗ {e:#}");
                self.ui(move |u| u.set_detect_status(s(m)));
            }
        }
    }

    async fn agent_config(&self, f: &AgentForm) -> Result<(AccountId, VaultId, whisker_agent::AgentConfig)> {
        let id: AccountId = f.account.parse()?;
        let vault = self.vault_of(id).await.ok_or_else(|| anyhow::anyhow!("that account is locked"))?;
        let old = agents_in(&self.mgr, &vault).await.into_iter().find(|c| c.account == id);
        let detected = self.st.lock().unwrap().detected.get(&id).cloned();
        let cfg = agent_form::from_form(id, f, old.as_ref(), detected.as_ref())?;
        Ok((id, vault, cfg))
    }

    async fn save_agent(&self, f: AgentForm) {
        let (id, vault, cfg) = match self.agent_config(&f).await {
            Ok(x) => x,
            Err(e) => return self.settings_msg(format!("{e:#}")),
        };
        let mut cfgs = agents_in(&self.mgr, &vault).await;
        cfgs.retain(|c| c.account != id);
        cfgs.push(cfg.clone());
        if let Err(e) = save_agents(&self.mgr, &vault, &cfgs).await {
            return self.settings_msg(format!("{e:#}"));
        }
        if let Some(h) = self.st.lock().unwrap().agents.remove(&id) {
            h.abort();
        }
        let msg = if cfg.enabled {
            match whisker_agent::spawn(cfg.clone(), self.mgr.clone()) {
                Ok(h) => {
                    self.st.lock().unwrap().agents.insert(id, h);
                    "Saved. The AI is running."
                }
                Err(_) => "Saved, but the AI couldn't start.",
            }
        } else {
            "Saved. The AI is turned off."
        };
        self.settings_msg(msg);
        let name = f.name.to_string();
        let form = agent_form::to_form(id, &name, Some(&cfg));
        self.ui(move |u| u.set_agent_form(form));
    }

    async fn test_agent(&self, f: AgentForm) {
        self.ui(|u| u.set_test_status(s("Asking the model…")));
        let msg = match self.agent_config(&f).await {
            Err(e) => format!("✗ {e:#}"),
            Ok((_, _, cfg)) => match Llm::new(cfg.api, cfg.max_tokens.min(300), cfg.extra_params) {
                Err(e) => format!("✗ {e:#}"),
                Ok(llm) => match llm.test().await {
                    Ok(reply) => format!("✓ It works. The model said: “{}”", reply.trim()),
                    Err(e) => format!("✗ {e:#}"),
                },
            },
        };
        self.ui(move |u| u.set_test_status(s(msg)));
    }

    async fn remove_agent(&self) {
        let Some(id) = self.settings_account() else { return };
        let Some(vault) = self.vault_of(id).await else { return };
        let mut cfgs = agents_in(&self.mgr, &vault).await;
        cfgs.retain(|c| c.account != id);
        if let Err(e) = save_agents(&self.mgr, &vault, &cfgs).await {
            return self.settings_msg(format!("{e:#}"));
        }
        if let Some(h) = self.st.lock().unwrap().agents.remove(&id) {
            h.abort();
        }
        self.open_settings(format!("ai:{id}")).await;
        self.settings_msg("AI setup removed. The account stays logged in.");
    }

    async fn change_passphrase(&self, old: String, new: String) {
        let account = self.st.lock().unwrap().account;
        let vault = match account {
            Some(a) => self.vault_of(a).await,
            None => self.st.lock().unwrap().active_vault.clone(),
        };
        let Some(vault) = vault else { return self.settings_msg("Unlock a vault first.") };
        self.settings_msg(match self.mgr.change_passphrase(&vault, &old, &new).await {
            Ok(()) => "Passphrase changed.".to_owned(),
            Err(e) => format!("{e:#}"),
        });
    }

    /// Locks everything once the user has been idle for the chosen time.
    async fn auto_lock_loop(self) {
        loop {
            tokio::time::sleep(Duration::from_secs(20)).await;
            let minutes = self.settings.lock().unwrap().auto_lock_minutes;
            let idle = self.st.lock().unwrap().last_activity.map(|t| t.elapsed());
            if minutes > 0 && idle.is_some_and(|d| d >= Duration::from_secs(minutes * 60)) {
                self.lock().await;
            }
        }
    }

    /// Feeds live events from every account into the UI.
    async fn pump(self) {
        let mut rx = self.mgr.subscribe();
        loop {
            let ev = match rx.recv().await {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            };
            let selected = |a: AccountId| self.st.lock().unwrap().account == Some(a);
            match ev {
                ClientEvent::AccountsChanged => self.refresh_accounts().await,
                ClientEvent::Synced { account } => {
                    let first = self.st.lock().unwrap().synced.insert(account);
                    if first {
                        self.refresh_accounts().await;
                        let c = self.clone();
                        tokio::spawn(async move {
                            c.mgr.fill_previews(account).await;
                            c.schedule_rooms();
                        });
                    } else if selected(account) {
                        self.schedule_rooms();
                    }
                }
                ClientEvent::Message { account, room_id, event_id, sender, sender_name, sender_avatar, body, origin_server_ts, reply_to, media, .. } => {
                    let key = (account, room_id);
                    let item = HistoryItem { event_id, sender, sender_name, sender_avatar, body, ts: origin_server_ts, reply_to, media: media.map(|m| *m), edited: false };
                    // Only keep live messages for rooms we've opened; others load on open.
                    let opened = self.st.lock().unwrap().older.contains_key(&key);
                    if opened {
                        self.merge(&key, vec![item]);
                        if self.current().as_ref() == Some(&key) {
                            self.append_last(&key).await;
                            let c = self.clone();
                            let k = key.clone();
                            self.spawn(async move { c.mark_read(&k).await });
                        }
                    }
                    if selected(account) {
                        self.schedule_rooms();
                    }
                }
                ClientEvent::Edited { account, room_id, target, body } => {
                    let key = (account, room_id);
                    let found = {
                        let mut st = self.st.lock().unwrap();
                        st.messages.get_mut(&key).and_then(|l| l.iter_mut().find(|m| m.event_id == target)).map(|m| {
                            m.body = body;
                            m.edited = true;
                        })
                    };
                    if found.is_some() {
                        self.update_message(&key, &target);
                    }
                }
                ClientEvent::Redacted { account, room_id, target } => self.remove_message(&(account, room_id), &target),
                ClientEvent::Verification { account, user, flow_id, state } => {
                    let c = self.clone();
                    self.spawn(async move { c.on_verification(account, user, flow_id, state).await });
                }
                ClientEvent::AccountError { message, .. } => self.status(tidy_error(&message)),
            }
        }
    }
}

/// Background errors, shortened for the status line: network errors quote
/// the whole request URL (access filters and all), which says nothing useful.
fn tidy_error(message: &str) -> String {
    let mut m = message.to_owned();
    while let Some(start) = m.find(" for url (") {
        let end = m[start..].find(')').map_or(m.len(), |e| start + e + 1);
        m.replace_range(start..end, "");
    }
    if m.starts_with("error sending request") {
        return "Can't reach the server right now; retrying.".to_owned();
    }
    m
}

pub fn run(data_dir: PathBuf) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let ui = AppWindow::new()?;
    ui::install(&ui);
    let mgr = AccountManager::new(data_dir);
    ui.set_first_run(mgr.is_empty());

    let prefs = AppSettings::load(mgr.data_dir());
    ui.set_theme(prefs.theme.index());
    ui.set_auto_lock(prefs.auto_lock_index());
    ui.set_encryption_summary(ENCRYPTION_SUMMARY.into());
    ui.global::<Prefs>().set_rich_text(prefs.rich_text);
    ui::RICH.set(prefs.rich_text);

    let ctl = Ctl {
        rt: rt.handle().clone(),
        mgr,
        ui: ui.as_weak(),
        st: Arc::default(),
        settings: Arc::new(Mutex::new(prefs)),
        avatars: Arc::default(),
        loading: Arc::default(),
    };

    // Runs `method` on the tokio runtime with the callback's arguments.
    macro_rules! on {
        ($setter:ident, |$($a:ident),*| $method:ident) => {{
            let c = ctl.clone();
            ui.$setter(move |$($a),*| {
                let c2 = c.clone();
                $(let $a = $a.to_owned();)*
                c.spawn(async move { c2.$method($($a.into()),*).await });
            });
        }};
        ($setter:ident, $method:ident) => {{
            let c = ctl.clone();
            ui.$setter(move || {
                let c2 = c.clone();
                c.spawn(async move { c2.$method().await });
            });
        }};
    }
    on!(on_unlock, |p| unlock);
    on!(on_unlock_more, |p| unlock_more);
    on!(on_create_hidden, |p| create_hidden);
    on!(on_select_account, |id| select_account);
    on!(on_select_room, |id| select_room);
    on!(on_search_rooms, |q| search_rooms);
    on!(on_send, |t| send);
    on!(on_message_action, |a, id| message_action);
    on!(on_profile_action, |a, user| profile_action);
    on!(on_profile_requested, |user| profile_requested);
    on!(on_verify_action, |a| verify_action);
    on!(on_profile_edit, |a, id, v| profile_edit);
    on!(on_open_room_info, open_room_info);
    on!(on_room_invite, |users| room_invite);
    on!(on_room_edit, |n, t| room_edit);
    on!(on_create_room, |n, t, public, alias, enc, inv| create_room);
    on!(on_login, |hs, u, p, t, px, ag, reg, tok| login);
    on!(on_account_menu, |a, id| account_menu);
    on!(on_room_menu, |a, id| room_menu);
    on!(on_explore_search, |srv, q| explore_search);
    on!(on_explore_join, |t| explore_join);
    on!(on_explore_more, explore_more);
    on!(on_open_settings, |id| open_settings);
    {
        // AccountForm holds an image (not Send): take the text out on the UI thread.
        let c = ctl.clone();
        ui.on_save_account(move |f| {
            let c2 = c.clone();
            let data = (f.id.to_string(), f.label.to_string(), f.tags.to_string(), f.proxy.to_string());
            c.spawn(async move { c2.save_account(data).await });
        });
    }
    on!(on_recover, |k| recover);
    on!(on_logout, |id| logout);
    on!(on_save_agent, |f| save_agent);
    on!(on_test_agent, |f| test_agent);
    on!(on_change_passphrase, |o, n| change_passphrase);
    on!(on_attach, attach);
    on!(on_load_older, load_older);
    on!(on_remove_agent, remove_agent);
    on!(on_lock, lock);
    {
        let c = ctl.clone();
        ui.on_detect_api(move |url, key| {
            let c2 = c.clone();
            let sel = c.ui.upgrade().map(|u| u.get_nav_selected().to_string()).unwrap_or_default();
            let Some(id) = sel.strip_prefix("ai:").and_then(|i| i.parse::<AccountId>().ok()) else { return };
            let (url, key) = (url.to_string(), key.to_string());
            c.spawn(async move { c2.detect_api(id, url, key).await });
        });
    }
    ui.on_open_link(|l| open_link(&l));
    {
        let c = ctl.clone();
        ui.on_filter_tag(move |t| c.cycle_tag(t.to_string()));
    }
    {
        let c = ctl.clone();
        ui.on_set_theme(move |i| c.set_theme(i));
    }
    {
        let c = ctl.clone();
        ui.on_set_auto_lock(move |i| c.set_auto_lock(i));
    }
    {
        let c = ctl.clone();
        ui.on_set_rich_text(move |b| c.set_rich_text(b));
    }
    {
        let c = ctl.clone();
        ui.on_activity(move || c.touch());
    }

    rt.spawn(ctl.clone().pump());
    rt.spawn(ctl.clone().auto_lock_loop());
    ui.run()?;
    rt.block_on(ctl.mgr.lock_all());
    Ok(())
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    let dir = app.internal_data_path().unwrap_or_else(|| PathBuf::from("/data/local/tmp/whisker"));
    slint::android::init(app).expect("Slint Android backend");
    if let Err(e) = run(dir) {
        eprintln!("whisker: {e:#}");
    }
}





