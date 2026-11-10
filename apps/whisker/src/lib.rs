//! Whisker GUI. The Slint UI runs on the main thread; all Matrix work runs
//! on a tokio runtime and pushes results back with `invoke_from_event_loop`.
#![recursion_limit = "256"]

mod agent_form;
mod autocorrect;
mod emoji;
mod notify;
#[cfg(target_os = "linux")]
mod player;
#[cfg(not(target_os = "android"))]
#[doc(hidden)]
pub mod select;
mod format;
mod settings;
pub mod share;
mod spaces;
mod spell;
#[cfg(not(target_os = "android"))]
mod spell_marks;
mod ui;

use anyhow::Result;
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::{MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedMxcUri, OwnedRoomId, OwnedUserId, UInt};
use settings::{AUTO_LOCK_MINUTES, AppSettings, ENCRYPTION_SUMMARY, Theme};
use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, VecModel, Weak};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use ui::{MsgRow, PixelCache, Pixels, initial_hue, s};
use whisker_agent::{Detected, Llm, agents_in, save_agents};
use whisker_core::files::{self, FileLink};
use whisker_core::stickers::{self, Sticker};
use whisker_core::timeline::{self, HistoryItem, MediaKind};
use whisker_core::{AccountId, AccountKind, AccountManager, AccountSummary, ClientEvent, LoginRequest, ModAction, VaultId};

slint::include_modules!();

const KEEP_PER_ROOM: usize = 1000;
const PAGE: u32 = 50;
/// Opening a room draws this many of its newest messages at once, then the
/// rest a few at a time, so a long room doesn't hold up the window.
const ROWS_FIRST: usize = 20;
const ROWS_STEP: usize = 15;
const AVATAR_PX: u32 = 96;
/// Animated pictures bigger than this show still (playing them means
/// downloading and decoding every frame).
const MAX_ANIMATION_BYTES: u64 = 12 * 1024 * 1024;
/// Biggest linked picture fetched to learn its size before sending.
const MAX_LINKED_PICTURE_BYTES: u64 = 50 * 1024 * 1024;
/// Biggest video without a poster frame that's fetched to make one.
const MAX_POSTER_VIDEO_BYTES: u64 = 200 * 1024 * 1024;

type RoomKey = (AccountId, OwnedRoomId);
/// A character card and its PNG picture (if it came in one).
type CardAndPicture = (whisker_agent::card::Card, Option<Vec<u8>>);

#[derive(Default)]
struct State {
    account: Option<AccountId>,
    room: Option<OwnedRoomId>,
    /// A room looked at before joining (it's `room` while shown), and
    /// where to go back to: Explore, or the room open before.
    preview: Option<(whisker_core::rooms::Preview, PreviewFrom)>,
    tag_filter: Option<String>,
    room_search: String,
    /// New logins go here: the vault unlocked or created most recently.
    active_vault: Option<VaultId>,
    messages: HashMap<RoomKey, Vec<HistoryItem>>,
    /// Pagination token for older history, per room (None: reached the start).
    older: HashMap<RoomKey, Option<String>>,
    /// While a room is opening: how many of its newest rows are drawn so far.
    row_limit: Option<usize>,
    /// Emoji reactions seen in each opened room.
    reactions: HashMap<RoomKey, Vec<timeline::Reaction>>,
    /// The message the "Forward to…" dialog is for.
    forwarding: Option<(RoomKey, OwnedEventId)>,
    /// Files and text shared from other apps, not sent yet, and the account
    /// picked to send them.
    sharing: Vec<share::Shared>,
    share_account: Option<AccountId>,
    /// The last typo fixed in the composer: (draft after the fix, draft
    /// before it, the word typed), so Backspace can undo it.
    autocorrected: Option<(String, String, String)>,
    /// The composer's text as of the last keystroke (UI thread's view).
    typed: String,
    /// Words put back with Backspace: not fixed again this session.
    no_fix: HashSet<String>,
    /// The rooms each space lists (from the server), with when they were fetched.
    space_rooms: HashMap<RoomKey, (Instant, Vec<timeline::SpaceRoom>)>,
    /// Rooms whose undecryptable messages are about to be fetched again.
    redecrypting: HashSet<RoomKey>,
    /// Running agents, by the account they speak as.
    agents: HashMap<AccountId, JoinHandle<()>>,
    last_activity: Option<Instant>,
    refresh_pending: bool,
    synced: HashSet<AccountId>,
    detected: HashMap<AccountId, Detected>,
    /// The last notification shown per room, so a new one replaces it.
    notified: HashMap<RoomKey, (u32, Instant)>,
    /// Who's typing, per room.
    typing: HashMap<RoomKey, Vec<OwnedUserId>>,
    /// When we last told the server we're typing (it times out after a few seconds).
    typing_sent: Option<(RoomKey, Instant)>,
    /// Room members for @-suggestions: (when fetched, (user, name)).
    members_cache: HashMap<RoomKey, (Instant, Vec<(OwnedUserId, String)>)>,
    /// The draft as last typed, and the people picked from @-suggestions.
    draft: String,
    mentions: Vec<(String, OwnedUserId)>,
    /// You picked the account yourself (so it isn't switched for you when
    /// the top account finishes loading after unlock).
    account_chosen: bool,
    /// The thread open in the side panel (its first message), in the open room.
    thread: Option<OwnedEventId>,
    /// Spaces shown folded (their rooms hidden), per account.
    closed_spaces: HashSet<OwnedRoomId>,
    /// Character cards imported (or removed: `None`) on an AI's settings
    /// page but not saved yet, with their PNG picture.
    pending_cards: HashMap<AccountId, Option<CardAndPicture>>,
    reply_to: Option<OwnedEventId>,
    /// Files picked to send with the next message (its text is their
    /// caption), in the open room.
    pending_files: Vec<PathBuf>,
    /// Your stickers, with the vault each is kept in.
    stickers: Vec<(VaultId, Sticker)>,
    /// The picture being saved as a sticker, while its name is asked.
    sticker_from: Option<timeline::Media>,
    /// Stickers' pictures, small (`None`: couldn't be loaded).
    sticker_pics: HashMap<String, Option<Pixels>>,
    editing: Option<OwnedEventId>,
    /// What the servers said about files linked in messages (`None`: they
    /// couldn't say), by address.
    linked: HashMap<OwnedMxcUri, Option<files::LinkedFile>>,
    /// Previews whose server didn't answer: how many times tried.
    picture_tries: HashMap<OwnedEventId, u8>,
    /// Decoded image previews, by event.
    pictures: HashMap<OwnedEventId, Option<Pixels>>,
    /// Read receipts per room, worked out when they change (it reads every
    /// member's receipt, too slow to redo on every redraw).
    marks: HashMap<RoomKey, Vec<timeline::ReadMark>>,
    /// Rooms whose receipts are being worked out again.
    marks_pending: HashSet<RoomKey>,
    /// Animated pictures (GIF, WebP): their frames and ms per frame.
    animations: HashMap<OwnedEventId, ui::Frames>,
    /// Whether each account has verified each person it sees messages from
    /// (see [`ui::Trust`]), looked up once and kept.
    trust: HashMap<(AccountId, OwnedUserId), ui::Trust>,
    /// Rooms marked read locally: the server's unread count at that moment.
    /// The badge stays hidden until the server reports a different count.
    cleared: HashMap<RoomKey, u64>,
    /// Messages from others that arrived in rooms that weren't open, since
    /// each was last read. The server's count only has the ones your push
    /// rules alert for, so rooms set to mentions only never showed new
    /// messages.
    fresh: HashMap<RoomKey, HashSet<OwnedEventId>>,
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
    /// Avatars whose download failed: how many times, for the wait before
    /// the next try.
    avatar_tries: Arc<Mutex<HashMap<OwnedMxcUri, u8>>>,
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

/// Where played videos and audio go: the RAM folder, wiped on lock and quit.
fn media_dir(data_dir: &std::path::Path) -> PathBuf {
    whisker_core::stores::runtime_root(data_dir).join("media")
}

/// Plays a video (looping, sound off to start, like videos in chat apps)
/// or audio in mpv when it's installed, else in the system's player.
fn play_file(path: &std::path::Path, video: bool) {
    let mut cmd = std::process::Command::new("mpv");
    cmd.arg("--force-window=yes").arg("--title=Whisker").arg("--no-terminal");
    if video {
        cmd.arg("--loop-file=inf").arg("--mute=yes");
    }
    if cmd.arg(path).spawn().is_err() {
        open_file(path);
    }
}

/// One way to get a message's preview: what to download (a thumbnail the
/// server makes, this many pixels across, or the file itself), and what it is.
type Attempt = (MediaSource, Option<u32>, Preview);

/// Times a preview is tried before it shows as failed.
const PREVIEW_TRIES: u8 = 4;

/// Previews downloading at once, across the whole app. Each batch used to
/// get its own few, so a busy room sent dozens of requests at once and
/// servers turned some down (rate limits), leaving blank previews.
static PREVIEW_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(6);
/// The same for avatars.
static AVATAR_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(6);

/// Biggest picture downloaded whole when no thumbnail can be had.
const MAX_FULL_PICTURE_BYTES: u64 = 25 * 1024 * 1024;

/// The ways to get `media`'s preview, best first: the sender's thumbnail,
/// one the server makes (it also turns pictures we can't read, like AVIF
/// or HEIC, into ones we can), then the file itself.
fn preview_attempts(media: &timeline::Media) -> Vec<Attempt> {
    let plain = matches!(media.source, MediaSource::Plain(_));
    let fits = |max: u64| media.size.is_none_or(|s| s <= max);
    let mut out = Vec::new();
    match media.kind {
        MediaKind::Image => {
            let animated = matches!(media.mime.as_deref(), Some("image/gif" | "image/webp")) && fits(MAX_ANIMATION_BYTES);
            if animated {
                out.push((media.source.clone(), None, Preview::Animated));
            }
            if let Some(t) = &media.thumbnail {
                out.push((t.clone(), None, Preview::Still));
            }
            if plain {
                out.push((media.source.clone(), Some(640), Preview::Still));
            }
            if !animated && fits(MAX_FULL_PICTURE_BYTES) {
                out.push((media.source.clone(), None, Preview::Still));
            }
        }
        MediaKind::Video => {
            if let Some(t) = &media.thumbnail {
                out.push((t.clone(), None, Preview::Still));
            }
            // Sent without a poster (or it won't load): make one from the
            // video, reading only what's needed of it where it can.
            if plain {
                out.push((media.source.clone(), None, Preview::Remote));
            }
            if fits(MAX_POSTER_VIDEO_BYTES) {
                out.push((media.source.clone(), None, Preview::Video));
            }
        }
        MediaKind::Audio | MediaKind::File => {}
    }
    out
}

/// The first line of a room description that has something on it.
fn first_line(topic: &str) -> &str {
    topic
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
}

/// A file's `mxc://` address, for messages.
fn media_address(src: &MediaSource) -> &str {
    match src {
        MediaSource::Plain(u) => u.as_str(),
        MediaSource::Encrypted(f) => f.url.as_str(),
    }
}

/// Where a room preview was opened from, to go back there on Close.
#[derive(Debug, Clone)]
enum PreviewFrom {
    Explore,
    Room(Option<OwnedRoomId>),
}

/// What a matrix.to link in a message points at.
#[derive(Debug, PartialEq)]
enum RoomLink {
    User(String),
    /// A room's id or address, and servers that know it.
    Room(String, Vec<String>),
}

fn room_link(url: &str) -> Option<RoomLink> {
    use matrix_sdk::ruma::MatrixToUri;
    use matrix_sdk::ruma::matrix_uri::MatrixId;
    let u = MatrixToUri::parse(url).ok()?;
    let via = u.via().iter().map(|v| v.to_string()).collect();
    match u.id() {
        MatrixId::User(user) => Some(RoomLink::User(user.to_string())),
        MatrixId::Room(id) => Some(RoomLink::Room(id.to_string(), via)),
        MatrixId::RoomAlias(alias) => Some(RoomLink::Room(alias.to_string(), via)),
        // A link to a message opens its room.
        MatrixId::Event(room, _) => Some(RoomLink::Room(room.to_string(), via)),
        _ => None,
    }
}

/// What a message preview is made from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Preview {
    Still,
    /// A GIF or WebP: every frame.
    Animated,
    /// A whole video, for a poster frame.
    Video,
    /// A poster frame read from a video on the server, through a relay
    /// (only the parts needed are fetched).
    Remote,
}

/// A poster frame (JPEG) for a downloaded video, via ffmpeg.
fn poster_of(video: &[u8]) -> Option<Vec<u8>> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("whisker-poster-{}-{n}", std::process::id()));
    std::fs::write(&path, video).ok()?;
    let poster = whisker_core::timeline::video_poster(&path);
    let _ = std::fs::remove_file(&path);
    poster.map(|p| p.jpeg)
}

/// Opens a downloaded file in the app the system picks for it.
fn open_file(path: &std::path::Path) {
    #[cfg(target_os = "linux")]
    let r = std::process::Command::new("xdg-open").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(path).spawn();
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    let r: std::io::Result<()> = Ok(());
    if let Err(e) = r {
        tracing::warn!("couldn't open the file: {e}");
    }
}

struct AccountRow {
    a: AccountSummary,
    unread: u64,
}
struct RoomRowData {
    depth: usize,
    space: bool,
    open: bool,
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
            let Some(client) = c.mgr.client(account).await else {
                return;
            };
            // Several at once: one slow server shouldn't hold up the rest.
            let mut jobs = tokio::task::JoinSet::new();
            for m in wanted {
                let (c, client) = (c.clone(), client.clone());
                jobs.spawn(async move {
                    let _slot = AVATAR_SLOTS.acquire().await;
                    // The server's small copy first; when it gives one we can't
                    // read (or none), the picture itself.
                    let thumb = timeline::thumbnail(&client, &m, AVATAR_PX).await;
                    let thumb_failed = thumb.is_err();
                    let mut px = match thumb {
                        Ok(b) => tokio::task::spawn_blocking(move || decode(&b, AVATAR_PX, AVATAR_PX, true)).await.ok().flatten(),
                        Err(_) => None,
                    };
                    let mut unreachable = thumb_failed;
                    if px.is_none() {
                        match timeline::download(&client, &MediaSource::Plain(m.clone()), None).await {
                            Ok(b) => px = tokio::task::spawn_blocking(move || decode(&b, AVATAR_PX, AVATAR_PX, true)).await.ok().flatten(),
                            Err(_) => unreachable = true,
                        }
                    }
                    drop(_slot);
                    match px {
                        Some(p) => {
                            c.avatars.lock().unwrap().insert(m.clone(), Some(p));
                            c.avatar_tries.lock().unwrap().remove(&m);
                            c.loading.lock().unwrap().remove(m.as_str());
                        }
                        // A download failed (busy server, rate limit, offline):
                        // try again by itself, sooner at first. Only a picture
                        // that came but can't be read is given up on.
                        None if unreachable => {
                            let tries = {
                                let mut t = c.avatar_tries.lock().unwrap();
                                let n = t.entry(m.clone()).or_insert(0);
                                *n = n.saturating_add(1);
                                *n
                            };
                            let wait = Duration::from_secs(match tries {
                                1 => 5,
                                2 => 20,
                                3 => 60,
                                _ => 300,
                            });
                            let c2 = c.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(wait).await;
                                c2.loading.lock().unwrap().remove(m.as_str());
                                c2.fetch_avatars(account, [m]);
                            });
                        }
                        None => {
                            c.avatars.lock().unwrap().insert(m.clone(), None);
                            c.loading.lock().unwrap().remove(m.as_str());
                        }
                    }
                });
            }
            while jobs.join_next().await.is_some() {}
            c.refresh_accounts_only().await;
            c.refresh_rooms().await;
            c.show_messages().await;
            c.show_explore().await;
        });
    }

    /// Fetches image previews for messages in the open room.
    /// Loads previews: image thumbnails, video poster frames, and whole
    /// animated GIF/WebP files (so they play). A few at a time, decoded off
    /// the network threads.
    fn fetch_pictures(&self, key: &RoomKey, items: &[HistoryItem]) {
        let items = &self.resolve_links(key, items);
        let wanted: Vec<(OwnedEventId, Vec<Attempt>)> = {
            let st = self.st.lock().unwrap();
            let mut loading = self.loading.lock().unwrap();
            // Newest first: those are the ones in view when a room opens.
            items
                .iter()
                .rev()
                .filter_map(|m| {
                    let media = m.media.as_ref()?;
                    if st.pictures.contains_key(&m.event_id) {
                        return None;
                    }
                    let attempts = preview_attempts(media);
                    (!attempts.is_empty() && loading.insert(m.event_id.to_string())).then(|| (m.event_id.clone(), attempts))
                })
                .collect()
        };
        if wanted.is_empty() {
            return;
        }
        let (c, account, room) = (self.clone(), key.0, key.1.clone());
        self.spawn(async move {
            let Some(client) = c.mgr.client(account).await else {
                return;
            };
            for (id, attempts) in wanted {
                let (c, client, room) = (c.clone(), client.clone(), room.clone());
                tokio::spawn(async move {
                    let _slot = PREVIEW_SLOTS.acquire().await;
                    // Each way in turn: another server may not make thumbnails,
                    // or send one we can't read.
                    let (mut decoded, mut failed) = ((None, None), false);
                    for (src, thumb, what) in attempts {
                        if let (Preview::Remote, MediaSource::Plain(mxc)) = (what, &src) {
                            let poster = files::look_at_video(&client, mxc, true).await.and_then(|(_, p)| p);
                            match poster {
                                Some(p) => {
                                    decoded = (tokio::task::spawn_blocking(move || decode(&p.jpeg, 720, 600, false)).await.ok().flatten(), None);
                                    if decoded.0.is_some() {
                                        break;
                                    }
                                }
                                None => tracing::debug!("couldn't read a frame of {mxc} through the relay"),
                            }
                            continue;
                        }
                        match timeline::download_in(&client, &room, &src, thumb).await {
                            Ok(b) => {
                                decoded = tokio::task::spawn_blocking(move || match what {
                                    Preview::Video => (poster_of(&b).and_then(|jpeg| decode(&jpeg, 720, 600, false)), None),
                                    Preview::Animated => (decode(&b, 720, 600, false), ui::decode_frames(&b, 720, 600)),
                                    Preview::Still | Preview::Remote => (decode(&b, 720, 600, false), None),
                                })
                                .await
                                .unwrap_or((None, None));
                                if decoded.0.is_some() {
                                    break;
                                }
                            }
                            Err(e) => {
                                failed = true;
                                tracing::warn!(
                                    "couldn't fetch a preview of {id} ({}): {e:#}",
                                    media_address(&src)
                                );
                            }
                        }
                    }
                    // A download failed: try again a little later, as a click
                    // on "try again" would. Busy servers, rate limits and
                    // files still on their way from another server all
                    // clear up on their own; answers like "not found" can
                    // too, while the account's server is still fetching it.
                    if decoded.0.is_none() && failed {
                        let tries = {
                            let mut st = c.st.lock().unwrap();
                            let n = st.picture_tries.entry(id.clone()).or_default();
                            *n += 1;
                            *n
                        };
                        if tries < PREVIEW_TRIES {
                            drop(_slot);
                            tokio::time::sleep(Duration::from_secs(
                                5 * 3u64.pow(u32::from(tries) - 1),
                            ))
                            .await;
                            c.loading.lock().unwrap().remove(id.as_str());
                            let key = (account, room);
                            let item = c.st.lock().unwrap().messages.get(&key).and_then(|l| l.iter().find(|m| m.event_id == id).cloned());
                            if let Some(item) = item {
                                c.fetch_pictures(&key, &[item]);
                            }
                            return;
                        }
                    }
                    {
                        let mut st = c.st.lock().unwrap();
                        st.pictures.insert(id.clone(), decoded.0);
                        if let Some(f) = decoded.1 {
                            st.animations.insert(id.clone(), f);
                        }
                    }
                    c.loading.lock().unwrap().remove(id.as_str());
                    c.update_message(&(account, room), &id);
                });
            }
        });
    }

    /// Forgets a preview that couldn't be loaded and tries again.
    fn retry_picture(&self, key: &RoomKey, m: HistoryItem) {
        {
            let mut st = self.st.lock().unwrap();
            st.pictures.remove(&m.event_id);
            st.picture_tries.remove(&m.event_id);
        }
        self.update_message(key, &m.event_id);
        self.fetch_pictures(key, &[m]);
    }

    /// Messages that are only a file's link show that file: asks its
    /// server what it is (once per file), fills that in, and returns
    /// `items` with what's already known filled in.
    fn resolve_links(&self, key: &RoomKey, items: &[HistoryItem]) -> Vec<HistoryItem> {
        let mut out = items.to_vec();
        let (mut ask, mut filled) = (Vec::new(), Vec::new());
        {
            let mut guard = self.st.lock().unwrap();
            let st = &mut *guard;
            for m in &mut out {
                let Some(media) = m.media.as_mut().filter(|x| x.linked) else { continue };
                let MediaSource::Plain(mxc) = &media.source else { continue };
                match st.linked.get(mxc) {
                    Some(Some(file)) if media.mime.is_none() => {
                        files::fill(media, file);
                        if let Some(x) = st.messages.get_mut(key).and_then(|l| l.iter_mut().find(|x| x.event_id == m.event_id)) {
                            x.media.clone_from(&m.media);
                        }
                        filled.push(m.event_id.clone());
                    }
                    Some(_) => {}
                    None => {
                        if self.loading.lock().unwrap().insert(mxc.to_string()) {
                            ask.push(mxc.clone());
                        }
                    }
                }
            }
        }
        // Known from before: redraw them as the file they link to.
        for id in &filled {
            self.update_message(key, id);
        }
        if ask.is_empty() {
            return out;
        }
        let (c, key) = (self.clone(), key.clone());
        self.spawn(async move {
            let Some(client) = c.mgr.client(key.0).await else { return };
            for mxc in ask {
                let file = files::probe(&client, &mxc).await.inspect_err(|e| tracing::warn!("couldn't look up {mxc}: {e:#}")).ok();
                c.loading.lock().unwrap().remove(mxc.as_str());
                let filled: Vec<HistoryItem> = {
                    let mut st = c.st.lock().unwrap();
                    st.linked.insert(mxc.clone(), file.clone());
                    let Some(file) = file else { continue };
                    let Some(list) = st.messages.get_mut(&key) else { continue };
                    list.iter_mut()
                        .filter(|m| m.media.as_ref().is_some_and(|x| x.linked && matches!(&x.source, MediaSource::Plain(u) if *u == mxc)))
                        .map(|m| {
                            if let Some(x) = m.media.as_mut() {
                                files::fill(x, &file);
                            }
                            m.clone()
                        })
                        .collect()
                };
                for m in &filled {
                    c.update_message(&key, &m.event_id);
                }
                c.fetch_pictures(&key, &filled);
            }
        });
        out
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
            if let Some(words) = self.mgr.extra::<Vec<String>>(v, spell::VAULT_KEY).await {
                spell::add_custom(words);
            }
            if let Some(list) = self.mgr.extra::<Vec<Sticker>>(v, stickers::VAULT_KEY).await {
                let mut st = self.st.lock().unwrap();
                st.stickers.retain(|(from, _)| from != v);
                st.stickers.extend(list.into_iter().map(|x| (v.clone(), x)));
            }
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
        let waiting = !self.st.lock().unwrap().sharing.is_empty();
        if waiting {
            self.open_share().await;
        }
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
        spell::clear_custom();
        let _ = std::fs::remove_dir_all(media_dir(self.mgr.data_dir()));
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
            u.set_pending_files(ModelRc::default());
            u.set_editing(s(""));
            u.set_pane(1);
            u.set_share_open(false);
            u.set_share_what(s(""));
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
            // Accounts come online one by one after unlock: until you pick
            // one, show the top one, even if another was ready first.
            let auto = !st.account_chosen && st.room.is_none() && list.first().map(|a| a.id) != st.account;
            if auto || st.account.is_none_or(|a| !list.iter().any(|x| x.id == a)) {
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
        // A room being previewed isn't in the list.
        let previewing = self.st.lock().unwrap().preview.as_ref().filter(|(p, _)| Some(&p.room_id) == room.as_ref()).map(|(p, _)| p.clone());
        let cleared = {
            let mut st = self.st.lock().unwrap();
            // Forget a local "read" once the server's count moves on.
            st.cleared.retain(|(a, r), n| *a != account || rooms.iter().any(|x| &x.room_id == r && x.unread == *n));
            (st.cleared.clone(), st.fresh.clone())
        };
        let (cleared, fresh) = cleared;
        let closed = self.st.lock().unwrap().closed_spaces.clone();
        let rows: Vec<RoomRowData> = spaces::tree(&rooms, &closed, &search)
            .into_iter()
            .map(|line| {
                let r = line.room;
                if r.is_space {
                    return RoomRowData {
                        depth: line.depth,
                        space: true,
                        open: line.open,
                        pinned: r.pinned,
                        id: r.room_id.to_string(),
                        name: r.name.clone(),
                        invite: false,
                        avatar: r.avatar.clone(),
                        preview: match line.rooms {
                            1 => "1 room".into(),
                            n => format!("{n} rooms"),
                        },
                        time: String::new(),
                        unread: line.unread,
                        encrypted: false,
                    };
                }
                let preview = r
                    .latest
                    .as_ref()
                    .map(|l| {
                        if timeline::is_undecryptable(&l.body) {
                            "Encrypted message".to_owned()
                        } else {
                            format!("{}: {}", l.sender_label(), l.body.lines().find(|x| !x.trim().is_empty()).unwrap_or_default())
                        }
                    })
                    .unwrap_or_else(|| if r.encrypted { "Encrypted room".into() } else { String::new() });
                let read = room.as_ref() == Some(&r.room_id) || cleared.contains_key(&(account, r.room_id.clone()));
                RoomRowData {
                    depth: line.depth,
                    space: false,
                    open: false,
                    pinned: r.pinned,
                    id: r.room_id.to_string(),
                    name: r.name.clone(),
                    invite: r.invite,
                    avatar: r.avatar.clone(),
                    preview,
                    time: r.latest.as_ref().map(|l| fmt_when(l.ts)).unwrap_or_default(),
                    unread: if read { 0 } else { r.unread }.max(fresh.get(&(account, r.room_id.clone())).map_or(0, |f| f.len() as u64)),
                    encrypted: r.encrypted,
                }
            })
            .collect();
        let (room_id, room_name, topic, invite) = match (current, previewing) {
            (Some(r), _) => (r.room_id.to_string(), r.name, r.topic.unwrap_or_default(), r.invite),
            (None, Some(p)) => (p.room_id.to_string(), p.name, p.topic.unwrap_or_default(), false),
            (None, None) => Default::default(),
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
                            depth: r.depth as i32,
                            space: r.space,
                            open: r.open,
                        }
                    })
                    .collect();
                ui::sync_rows(&m.rooms, items, |a, b| a.id == b.id);
            });
            u.set_loading_rooms(!synced);
            u.set_current_room(s(room_id));
            u.set_current_invite(invite);
            u.set_current_room_name(s(room_name));
            u.set_current_room_topic(s(first_line(&topic)));
            u.set_current_room_topic_full(s(&topic));
        });
    }

    // ---- messages ----

    /// Builds display rows for a room's cached messages (thread replies
    /// left out: they show in the thread panel).
    async fn rows(&self, key: &RoomKey) -> Vec<MsgRow> {
        self.rows_for(key, None).await
    }

    /// Rows for the main timeline (`thread` None) or for one thread: its
    /// first message and the replies.
    async fn rows_for(&self, key: &RoomKey, thread: Option<&OwnedEventId>) -> Vec<MsgRow> {
        let client = self.mgr.client(key.0).await;
        let me: Option<OwnedUserId> = client.as_ref().and_then(|c| c.user_id().map(ToOwned::to_owned));
        let marks = self.st.lock().unwrap().marks.get(key).cloned().unwrap_or_default();
        let show_events = self.settings.lock().unwrap().show_events;
        let st = self.st.lock().unwrap();
        let mut msgs = st.messages.get(key).cloned().unwrap_or_default();
        // Thread replies: how many each thread has, and the newest.
        let mut threads: HashMap<OwnedEventId, (usize, String)> = HashMap::new();
        for m in &msgs {
            if let Some(root) = &m.thread {
                let t = threads.entry(root.clone()).or_default();
                t.0 += 1;
                t.1 = format!("{}: {}", m.sender_label(), m.body.lines().find(|l| !l.trim().is_empty()).unwrap_or_default());
            }
        }
        match thread {
            None => msgs.retain(|m| m.thread.is_none()),
            Some(root) => msgs.retain(|m| &m.event_id == root || m.thread.as_ref() == Some(root)),
        }
        drop_repeat_joins(&mut msgs);
        if !show_events {
            msgs.retain(|m| !m.notice);
        }
        let by_id: HashMap<&OwnedEventId, &HistoryItem> = msgs.iter().map(|m| (&m.event_id, m)).collect();
        let names: HashMap<&OwnedUserId, &str> = msgs.iter().map(|m| (&m.sender, m.sender_label())).collect();
        // Reactions per message: each emoji once, in the order first used.
        let mut reactions: HashMap<&OwnedEventId, Vec<(String, usize, bool, String)>> = HashMap::new();
        let mut counted = HashSet::new();
        for r in st.reactions.get(key).map(Vec::as_slice).unwrap_or_default() {
            if !counted.insert((&r.target, &r.key, &r.sender)) {
                continue;
            }
            let list = reactions.entry(&r.target).or_default();
            let who = names.get(&r.sender).map(|n| n.to_string()).unwrap_or_else(|| r.sender.localpart().to_owned());
            match list.iter_mut().find(|x| x.0 == r.key) {
                Some(x) => {
                    x.1 += 1;
                    x.2 |= me.as_ref() == Some(&r.sender);
                    x.3 = format!("{}, {who}", x.3);
                }
                None => list.push((r.key.clone(), 1, me.as_ref() == Some(&r.sender), who)),
            }
        }
        // Each reader goes on the message they read up to, or (if that one
        // isn't shown, e.g. a hidden room event) the last one before it.
        let mut readers: HashMap<usize, Vec<(String, String, Option<OwnedMxcUri>)>> = HashMap::new();
        for mark in &marks {
            let at = msgs.iter().position(|m| m.event_id == mark.event_id).or_else(|| {
                let ts = mark.ts?;
                msgs.iter().rposition(|m| m.ts <= ts)
            });
            if let Some(i) = at {
                let name = mark.name.clone().unwrap_or_else(|| mark.user.localpart().to_owned());
                readers.entry(i).or_default().push((mark.user.to_string(), name, mark.avatar.clone()));
            }
        }
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
                    || prev.is_none_or(|p| p.notice || p.sender != m.sender || i64::from(m.ts.get()) - i64::from(p.ts.get()) > 5 * 60 * 1000);
                let reply = m.reply_to.as_ref().map(|r| match by_id.get(r) {
                    Some(o) => (o.sender_label().to_owned(), o.body.lines().find(|l| !l.trim().is_empty()).unwrap_or_default().to_owned()),
                    None => ("↪".to_owned(), "an earlier message".to_owned()),
                });
                MsgRow {
                    picture: st.pictures.get(&m.event_id).cloned().flatten(),
                    picture_failed: matches!(st.pictures.get(&m.event_id), Some(None)),
                    frames: st.animations.get(&m.event_id).cloned(),
                    mine: me.as_ref() == Some(&m.sender),
                    first,
                    day,
                    reply,
                    readers: readers.remove(&i).unwrap_or_default(),
                    reactions: reactions.remove(&m.event_id).unwrap_or_default(),
                    thread: if thread.is_none() { threads.remove(&m.event_id) } else { None },
                    trust: st.trust.get(&(key.0, m.sender.clone())).copied().unwrap_or_default(),
                    item: m.clone(),
                }
            })
            .collect()
    }

    /// Redraws the whole open room (on switching rooms or loading history).
    async fn show_messages(&self) {
        self.show_messages_counted().await;
    }

    /// Shows the rest of the open room's rows a few at a time, older ones
    /// going in above the ones already there.
    async fn grow_messages(&self, key: &RoomKey) {
        loop {
            if self.current().as_ref() != Some(key) {
                return;
            }
            let total = self.show_messages_counted().await;
            let more = {
                let mut st = self.st.lock().unwrap();
                match st.row_limit {
                    Some(n) if n < total => {
                        st.row_limit = Some(n + ROWS_STEP);
                        true
                    }
                    _ => {
                        st.row_limit = None;
                        false
                    }
                }
            };
            if !more {
                return;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }

    /// Draws the open room; says how many rows it has in all (more than are
    /// drawn while it's still opening).
    async fn show_messages_counted(&self) -> usize {
        let Some(key) = self.current() else {
            self.ui(|_| {
                ui::with(|m| m.messages.set_vec(Vec::new()));
            });
            return 0;
        };
        let mut rows = self.rows(&key).await;
        let total = rows.len();
        if let Some(n) = self.st.lock().unwrap().row_limit
            && total > n
        {
            rows.drain(..total - n);
        }
        self.fetch_trust(key.0, rows.iter().filter(|r| !r.mine && !r.item.notice).map(|r| r.item.sender.clone()));
        self.fetch_avatars(key.0, rows.iter().filter_map(|r| r.item.sender_avatar.clone()).chain(rows.iter().flat_map(|r| r.readers.iter().filter_map(|x| x.2.clone()))));
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
        total
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
            c.show_thread().await;
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

    /// Adds messages to a room's cache; returns how many were new.
    fn merge(&self, key: &RoomKey, items: Vec<HistoryItem>) -> usize {
        let mut st = self.st.lock().unwrap();
        let list = st.messages.entry(key.clone()).or_default();
        let known: HashSet<_> = list.iter().map(|m| m.event_id.clone()).collect();
        let before = list.len();
        list.extend(items.into_iter().filter(|m| !known.contains(&m.event_id)));
        let added = list.len() - before;
        list.sort_by_key(|m| m.ts);
        if list.len() > KEEP_PER_ROOM {
            list.drain(..list.len() - KEEP_PER_ROOM);
        }
        added
    }

    /// New keys came for a room: messages shown as undecryptable there are
    /// fetched again (a burst of keys is handled once, shortly after).
    fn redecrypt(&self, key: RoomKey) {
        {
            let mut st = self.st.lock().unwrap();
            let locked = st.messages.get(&key).is_some_and(|l| l.iter().any(|m| timeline::is_undecryptable(&m.body)));
            if !locked || !st.redecrypting.insert(key.clone()) {
                return;
            }
        }
        let c = self.clone();
        self.spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            c.st.lock().unwrap().redecrypting.remove(&key);
            let Some(room) = c.mgr.client(key.0).await.and_then(|cl| cl.get_room(&key.1)) else { return };
            let Ok(page) = timeline::history(&room, None, PAGE).await else { return };
            c.mgr.note_history(key.0, &key.1, &page.items);
            let changed: Vec<OwnedEventId> = {
                let mut st = c.st.lock().unwrap();
                let Some(list) = st.messages.get_mut(&key) else { return };
                let mut changed = Vec::new();
                for new in page.items {
                    if let Some(old) = list.iter_mut().find(|m| m.event_id == new.event_id && timeline::is_undecryptable(&m.body))
                        && !timeline::is_undecryptable(&new.body)
                    {
                        changed.push(new.event_id.clone());
                        *old = new;
                    }
                }
                changed
            };
            for id in &changed {
                c.update_message(&key, id);
            }
            if !changed.is_empty() {
                c.refresh_rooms().await;
            }
        });
    }

    fn merge_reactions(&self, key: &RoomKey, new: Vec<timeline::Reaction>) {
        let mut st = self.st.lock().unwrap();
        let list = st.reactions.entry(key.clone()).or_default();
        for r in new {
            if !list.iter().any(|x| x.event_id == r.event_id) {
                list.push(r);
            }
        }
    }

    async fn select_account(&self, id: String) {
        let Ok(id) = id.parse::<AccountId>() else { return };
        {
            let mut st = self.st.lock().unwrap();
            st.account_chosen = true;
            if st.account == Some(id) {
                return;
            }
            st.account = Some(id);
            st.room = None;
            st.reply_to = None;
            st.pending_files.clear();
            st.editing = None;
            st.explore = Default::default();
        }
        self.ui(|u| {
            ui::with(|m| m.messages.set_vec(Vec::new()));
            u.set_reply_to(s(""));
            u.set_pending_files(ModelRc::default());
            u.set_editing(s(""));
            u.set_public_rooms(ModelRc::default());
            u.set_exploring(false);
        });
        // The room list first: that's what the click was for.
        self.refresh_rooms().await;
        self.refresh_accounts_only().await;
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
        self.end_preview();
        self.st.lock().unwrap().row_limit = None;
        // A video playing in the room being left stops.
        #[cfg(target_os = "linux")]
        player::stop(&self.ui);
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
                st.pending_files.clear();
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
        // A space opens as a page of its rooms (clicking it again folds it).
        if timeline::is_space(&room) {
            {
                let mut st = self.st.lock().unwrap();
                if st.room.as_ref() == Some(&room_id) {
                    if !st.closed_spaces.remove(&room_id) {
                        st.closed_spaces.insert(room_id.clone());
                    }
                } else {
                    st.closed_spaces.remove(&room_id);
                }
                st.room = Some(room_id.clone());
                st.reply_to = None;
                st.pending_files.clear();
                st.editing = None;
                st.thread = None;
            }
            self.ui(|u| {
                ui::with(|m| m.messages.set_vec(Vec::new()));
                u.set_current_space(true);
                u.set_thread_open(false);
                u.set_loading_history(false);
                u.set_reply_to(s(""));
                u.set_pending_files(ModelRc::default());
                u.set_editing(s(""));
                u.set_space_status(s(""));
            });
            self.refresh_rooms().await;
            return self.show_space(&key, &room).await;
        }
        self.ui(|u| u.set_current_space(false));
        // Encrypted rooms need the full member list before the first send;
        // fetch it now so pressing Enter doesn't wait for it.
        if room.encryption_state().is_encrypted() {
            let r = room.clone();
            tokio::spawn(async move {
                let started = Instant::now();
                if r.sync_members().await.is_ok() {
                    tracing::debug!(room = %r.room_id(), ms = started.elapsed().as_millis() as u64, "members fetched ahead of sending");
                }
            });
        }
        {
            let mut st = self.st.lock().unwrap();
            st.room = Some(room_id.clone());
            st.reply_to = None;
            st.pending_files.clear();
            st.editing = None;
            st.thread = None;
            st.row_limit = Some(ROWS_FIRST);
        }
        self.ui(move |u| {
            u.set_thread_open(false);
            u.set_loading_history(true);
            u.set_reply_to(s(""));
            u.set_pending_files(ModelRc::default());
            u.set_editing(s(""));
            u.set_stick_bottom(true);
        });
        // What we already have goes up first; the rest follows.
        self.show_messages().await;
        self.show_typing(&key).await;
        self.schedule_marks(key.clone());
        // Can our account kick/ban/delete others' messages here?
        let can_moderate = match (room.power_levels().await, client.user_id()) {
            (Ok(pl), Some(me)) => pl.user_can_kick(me) || pl.user_can_ban(me) || pl.user_can_redact_event_of_other(me),
            _ => false,
        };
        self.ui(move |u| u.set_can_moderate(can_moderate));
        self.refresh_rooms().await;

        let fetched = self.st.lock().unwrap().older.contains_key(&key);
        if !fetched {
            match timeline::history(&room, None, PAGE).await {
                Ok(page) => {
                    self.mgr.note_history(account, &room_id, &page.items);
                    self.merge(&key, page.items);
                    self.merge_reactions(&key, page.reactions);
                    self.st.lock().unwrap().older.insert(key.clone(), page.older);
                    if self.current().as_ref() == Some(&key) {
                        self.show_messages().await;
                    }
                }
                Err(e) => self.status(format!("Couldn't load history: {e}")),
            }
        }
        self.ui(|u| u.set_loading_history(false));
        self.grow_messages(&key).await;
        self.mark_read(&key).await;
    }

    /// Clears a room's unread badge now and tells the server (privately).
    async fn mark_read(&self, key: &RoomKey) {
        if let Some(n) = self.mgr.rooms(key.0).await.iter().find(|r| r.room_id == key.1).map(|r| r.unread) {
            self.st.lock().unwrap().cleared.insert(key.clone(), n);
        }
        self.st.lock().unwrap().fresh.remove(key);
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
        let Some(room) = self.mgr.client(account).await.and_then(|c| c.get_room(&room_id)) else {
            // A space's room you haven't joined: only its address.
            if action == "copy" {
                let link = format!("https://matrix.to/#/{room_id}");
                self.ui(move |u| u.invoke_copy_text(s(link)));
            }
            return;
        };
        let key = (account, room_id.clone());
        match action.as_str() {
            "toggle" => {
                {
                    let mut st = self.st.lock().unwrap();
                    if !st.closed_spaces.remove(&room_id) {
                        st.closed_spaces.insert(room_id);
                    }
                }
                self.refresh_rooms().await;
            }
            "pin" | "unpin" => match room.set_is_favourite(action == "pin", None).await {
                Ok(()) => self.schedule_rooms(),
                Err(e) => self.status(format!("Couldn't change pin: {e}")),
            },
            "accept" => {
                self.status("Joining…");
                match whisker_core::rooms::accept_invite(&room).await {
                    Ok(()) => {
                        timeline::check_room_type(&room).await;
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
                            topic: s(first_line(r.topic.as_deref().unwrap_or_default())),
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

    /// Shows a room (`!id` or `#alias`) without joining it: its name, topic
    /// and, when its history is open to anyone, its newest messages, with a
    /// Join button where the message box goes. `via`: servers that know it.
    async fn open_preview(&self, target: String, via: Vec<String>) {
        let Some(account) = self.st.lock().unwrap().account else { return };
        let Some(client) = self.mgr.client(account).await else { return };
        let joined = |id: &OwnedRoomId| client.get_room(id).is_some_and(|r| r.state() == matrix_sdk::RoomState::Joined);
        let from = {
            let exploring = self.ui.upgrade().is_some_and(|u| u.get_exploring());
            if exploring { PreviewFrom::Explore } else { PreviewFrom::Room(self.st.lock().unwrap().room.clone()) }
        };
        let show_room = |id: OwnedRoomId| {
            self.ui(|u| {
                u.set_exploring(false);
                u.set_pane(2);
                u.set_stick_bottom(true);
            });
            self.select_room(id.to_string())
        };
        if let Ok(id) = OwnedRoomId::try_from(target.trim())
            && joined(&id)
        {
            return show_room(id).await;
        }
        let say = |m: String| {
            self.ui(move |u| {
                u.set_explore_status(s(&m));
                u.set_space_status(s(&m));
            })
        };
        say("Looking at the room…".into());
        let via = via.iter().filter_map(|v| matrix_sdk::ruma::OwnedServerName::try_from(v.as_str()).ok()).collect();
        let p = match whisker_core::rooms::preview(&client, &target, via).await {
            Ok(p) => p,
            Err(e) => return say(format!("Couldn't preview it: {e:#}")),
        };
        say(String::new());
        if joined(&p.room_id) {
            return show_room(p.room_id).await;
        }
        #[cfg(target_os = "linux")]
        player::stop(&self.ui);
        self.end_preview();
        let key = (account, p.room_id.clone());
        let info = [
            Some(match p.members {
                1 => "1 member".to_owned(),
                n => format!("{n} members"),
            }),
            p.alias.clone(),
            p.encrypted.then(|| "encrypted".to_owned()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        let note = p.note.clone().unwrap_or_default();
        let (id, name, topic) = (p.room_id.to_string(), p.name.clone(), p.topic.clone().unwrap_or_default());
        {
            let mut st = self.st.lock().unwrap();
            st.room = Some(p.room_id.clone());
            st.reply_to = None;
            st.pending_files.clear();
            st.editing = None;
            st.thread = None;
            st.messages.insert(key.clone(), p.messages.clone());
            st.reactions.insert(key, p.reactions.clone());
            st.preview = Some((p, from));
        }
        self.ui(move |u| {
            u.set_exploring(false);
            u.set_pane(2);
            u.set_current_space(false);
            u.set_current_invite(false);
            u.set_current_preview(true);
            u.set_preview_info(s(info));
            u.set_preview_note(s(note));
            u.set_preview_busy(false);
            u.set_current_room(s(&id));
            u.set_current_room_name(s(name));
            u.set_current_room_topic(s(first_line(&topic)));
            u.set_current_room_topic_full(s(&topic));
            u.set_thread_open(false);
            u.set_loading_history(false);
            u.set_has_older(false);
            u.set_reply_to(s(""));
            u.set_pending_files(ModelRc::default());
            u.set_editing(s(""));
            u.set_can_moderate(false);
            u.set_stick_bottom(true);
        });
        self.show_messages().await;
    }

    /// Forgets the room being previewed (and its messages), if any.
    fn end_preview(&self) -> Option<(whisker_core::rooms::Preview, PreviewFrom)> {
        let ended = {
            let mut st = self.st.lock().unwrap();
            let ended = st.preview.take();
            if let (Some((p, _)), Some(account)) = (&ended, st.account) {
                let key = (account, p.room_id.clone());
                st.messages.remove(&key);
                st.reactions.remove(&key);
                if st.room.as_ref() == Some(&p.room_id) {
                    st.room = None;
                }
            }
            ended
        };
        if ended.is_some() {
            self.ui(|u| u.set_current_preview(false));
        }
        ended
    }

    /// "Join" on a preview.
    async fn preview_join(&self) {
        let Some(account) = self.st.lock().unwrap().account else { return };
        let Some(client) = self.mgr.client(account).await else { return };
        let Some((p, _)) = self.st.lock().unwrap().preview.clone() else { return };
        self.ui(|u| u.set_preview_busy(true));
        let target: &matrix_sdk::ruma::RoomOrAliasId = (&*p.room_id).into();
        match timeline::join_patiently(&client, target, &p.via).await {
            Ok(room) => {
                timeline::check_room_type(&room).await;
                self.end_preview();
                self.ui(|u| u.set_stick_bottom(true));
                self.select_room(p.room_id.to_string()).await;
            }
            Err(e) => {
                let m = format!("Couldn't join: {e:#}");
                self.ui(move |u| {
                    u.set_preview_busy(false);
                    u.set_preview_note(s(m));
                });
            }
        }
    }

    /// Leaves a preview without joining, back to where it was opened.
    async fn preview_close(&self) {
        let Some((_, from)) = self.end_preview() else { return };
        match from {
            PreviewFrom::Explore => {
                self.ui(|u| {
                    u.set_current_room(s(""));
                    u.set_exploring(true);
                });
                self.show_messages().await;
                self.refresh_rooms().await;
            }
            PreviewFrom::Room(Some(id)) => self.select_room(id.to_string()).await,
            PreviewFrom::Room(None) => {
                self.ui(|u| u.set_current_room(s("")));
                self.show_messages().await;
                self.refresh_rooms().await;
            }
        }
    }

    /// "Preview" on a room listed on the open space's page.
    async fn space_preview(&self, id: String) {
        let Some((key, _)) = self.current_room().await else { return };
        let via = self.st.lock().unwrap().space_rooms.get(&key).and_then(|(_, l)| l.iter().find(|r| r.room_id.as_str() == id).map(|r| r.via.iter().map(|v| v.to_string()).collect()));
        self.open_preview(id, via.unwrap_or_default()).await;
    }

    /// "Preview" on a room in Explore: its directory's server knows it.
    async fn explore_preview(&self, id: String) {
        let server = self.st.lock().unwrap().explore.0.clone();
        let via = whisker_core::rooms::normalize_server(&server).into_iter().collect();
        self.open_preview(id, via).await;
    }

    async fn profile_edit(&self, action: String, id: String, value: String) {
        let Ok(aid) = id.parse::<AccountId>() else { return };
        let Some(client) = self.mgr.client(aid).await else { return };
        let r = match action.as_str() {
            "make-ai" | "make-human" => {
                let ai = action == "make-ai";
                if !ai && let Some(h) = self.st.lock().unwrap().agents.remove(&aid) {
                    h.abort();
                }
                match self.mgr.set_kind(aid, if ai { AccountKind::Agent } else { AccountKind::Human }).await {
                    Ok(()) if ai => {
                        self.mgr.vouch_own().await;
                        self.open_settings(format!("ai:{aid}")).await;
                        self.settings_msg("Now pick the AI's API and model, then turn it on.");
                    }
                    Ok(()) => {
                        self.open_settings(format!("account:{aid}")).await;
                        self.settings_msg("This account is a normal account again.");
                    }
                    Err(e) => self.settings_msg(format!("{e:#}")),
                }
                return;
            }
            "name" => whisker_core::rooms::set_display_name(&client, &value).await,
            "remove-avatar" => whisker_core::rooms::set_avatar(&client, None).await,
            "export-keys" => {
                if value.chars().count() < 8 {
                    return self.settings_msg("Use a passphrase of at least 8 characters; you'll need it to import the file.");
                }
                let Some(path) = save_file_as("Export room keys", "whisker-keys.txt".into()).await else {
                    #[cfg(target_os = "android")]
                    self.settings_msg("Exporting keys isn't available on Android yet.");
                    return;
                };
                self.settings_msg("Exporting keys…");
                self.settings_msg(match client.encryption().export_room_keys(path.clone(), &value, |_| true).await {
                    Ok(()) => {
                        whisker_core::vault::owner_only(&path, false);
                        format!("Keys exported to {}. Keep the file and its passphrase safe: together they unlock your messages.", path.display())
                    }
                    Err(e) => format!("Couldn't export keys: {e:#}"),
                });
                return;
            }
            "import-keys" => {
                if value.is_empty() {
                    return self.settings_msg("Enter the passphrase the key file was exported with.");
                }
                let Some(path) = pick_path("Import room keys").await else {
                    #[cfg(target_os = "android")]
                    self.settings_msg("Importing key files isn't available on Android yet.");
                    return;
                };
                self.settings_msg("Importing keys…");
                self.settings_msg(match client.encryption().import_room_keys(path, &value).await {
                    Ok(r) => format!("Imported {} of {} keys. Older encrypted messages will show when you reopen a room.", r.imported_count, r.total_count),
                    Err(e) => format!("Couldn't import keys (is the passphrase right?): {e}"),
                });
                let mut st = self.st.lock().unwrap();
                st.messages.retain(|(a, _), _| *a != aid);
                st.older.retain(|(a, _), _| *a != aid);
                return;
            }
            "secure" | "reset-identity" => {
                use whisker_core::verify;
                self.settings_msg("Setting up verification…");
                let r = if action == "secure" {
                    verify::ensure_identity(&client, Some(&value)).await.and_then(|ok| if ok { Ok(()) } else { Err(anyhow::anyhow!("the server wants your password")) })
                } else {
                    verify::reset_identity(&client, &value).await
                };
                if let Err(e) = r {
                    return self.settings_msg(format!("Couldn't set up verification (is the password right?): {e:#}"));
                }
                self.mgr.vouch_own().await;
                // A recovery key straight away, so the next login can verify itself.
                let key = verify::enable_recovery(&client).await;
                self.open_settings(format!("account:{aid}")).await;
                match key {
                    Ok(k) => {
                        self.show_recovery_key(k);
                        self.settings_msg("This session is verified.");
                    }
                    Err(e) => self.settings_msg(format!("This session is verified, but the recovery key couldn't be made: {e:#}")),
                }
                return;
            }
            "make-recovery" => {
                self.settings_msg("Making a recovery key and backing up your keys…");
                let key = whisker_core::verify::enable_recovery(&client).await;
                self.open_settings(format!("account:{aid}")).await;
                match key {
                    Ok(k) => self.show_recovery_key(k),
                    Err(e) => self.settings_msg(format!("Couldn't make a recovery key: {e:#}")),
                }
                return;
            }
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

    /// Shows a just-made recovery key on the account page.
    fn show_recovery_key(&self, key: Option<String>) {
        let Some(key) = key else { return };
        self.ui(move |u| {
            let mut f = u.get_account_form();
            f.recovery_key = s(&key);
            f.recovery_on = true;
            u.set_account_form(f);
        });
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
        if tag == "done" {
            // Look them up again: they're verified now.
            self.st.lock().unwrap().trust.retain(|(a, u), _| (*a, u) != (account, &user));
        }
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
                can_set_avatar: d.can_set_avatar,
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

    /// "change" picks a picture for the open room's icon; "remove" clears it.
    async fn room_avatar(&self, action: String) {
        let Some((_, room)) = self.current_room().await else { return };
        let r = match action.as_str() {
            "remove" => whisker_core::rooms::set_room_avatar(&room, None).await,
            #[cfg(not(target_os = "android"))]
            "change" => {
                let picked = tokio::task::spawn_blocking(|| {
                    rfd::FileDialog::new().set_title("Choose a room picture").add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif"]).pick_file()
                })
                .await
                .ok()
                .flatten();
                let Some(path) = picked else { return };
                self.ui(|u| u.set_room_info_status(s("Uploading picture…")));
                whisker_core::rooms::set_room_avatar(&room, Some(&path)).await
            }
            #[cfg(target_os = "android")]
            "change" => Err(anyhow::anyhow!("changing room pictures isn't available on Android yet")),
            _ => return,
        };
        match r {
            Ok(()) => {
                self.open_room_info().await;
                self.ui(|u| u.set_room_info_status(s("Picture updated.")));
                self.schedule_rooms();
            }
            Err(e) => {
                let m = format!("Couldn't change the picture: {e:#}");
                self.ui(move |u| u.set_room_info_status(s(m)));
            }
        }
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
                let added = self.merge(&key, page.items);
                self.merge_reactions(&key, page.reactions);
                // Nothing new came back: there's nothing older to show.
                self.st.lock().unwrap().older.insert(key.clone(), if added == 0 { None } else { page.older });
                self.show_messages().await;
            }
            Err(e) => self.status(format!("Couldn't load older messages: {e}")),
        }
        self.ui(|u| u.set_loading_history(false));
    }

    async fn send(&self, text: String) {
        let text = emoji::replace_open_shortcodes(text.trim());
        let Some((key, room)) = self.current_room().await else {
            return;
        };
        // Files waiting to go: the text is their caption.
        let files = {
            let mut st = self.st.lock().unwrap();
            if st.editing.is_none() {
                std::mem::take(&mut st.pending_files)
            } else {
                Vec::new()
            }
        };
        if !files.is_empty() {
            let reply_to = self.st.lock().unwrap().reply_to.take();
            self.ui(|u| {
                u.set_reply_to(s(""));
                u.set_pending_files(ModelRc::default());
                u.set_stick_bottom(true);
            });
            if self.st.lock().unwrap().typing_sent.take().is_some() {
                let r = room.clone();
                tokio::spawn(async move { r.typing_notice(false).await });
            }
            self.send_files(room, files, text, reply_to).await;
            return;
        }
        if text.is_empty() {
            return;
        }
        // Only `:name:` of one of your stickers: that sticker.
        let sticker = {
            let st = self.st.lock().unwrap();
            if st.editing.is_none() { st.stickers.iter().map(|(_, x)| x).find(|x| stickers::find(std::slice::from_ref(x), &text).is_some()).cloned() } else { None }
        };
        if let Some(sticker) = sticker {
            self.post_sticker(&room, sticker).await;
            return;
        }
        // The clear command opens its confirmation instead of being posted.
        let clear = self.settings.lock().unwrap().clear_command.trim().to_owned();
        if !clear.is_empty() && text.eq_ignore_ascii_case(&clear) && self.st.lock().unwrap().editing.is_none() {
            let allowed = match (room.power_levels().await, room.client().user_id()) {
                (Ok(pl), Some(me)) => pl.user_can_redact_event_of_other(me),
                _ => false,
            };
            if allowed {
                self.ui(|u| u.set_clear_open(true));
            } else {
                self.status("Only moderators can clear messages here.");
            }
            return;
        }
        if self.st.lock().unwrap().typing_sent.take().is_some() {
            let r = room.clone();
            tokio::spawn(async move { r.typing_notice(false).await });
        }
        let (reply_to, editing) = {
            let mut st = self.st.lock().unwrap();
            (st.reply_to.take(), st.editing.take())
        };
        self.ui(|u| {
            u.set_reply_to(s(""));
            u.set_pending_files(ModelRc::default());
            u.set_editing(s(""));
            u.set_status(s(""));
        });

        if let Some(target) = editing {
            // A picture, video or file: its caption changes, the file stays.
            let file = self.find(&key, target.as_str()).is_some_and(|m| m.media.as_ref().is_some_and(|x| !x.linked));
            let done = if file { timeline::edit_caption(&room, &target, &text).await } else { timeline::edit_text(&room, &target, &text).await };
            if let Err(e) = done {
                self.status(format!("Not saved: {e:#}"));
            }
            return;
        }
        // A file's link goes as that file, nothing uploaded.
        if let Some(link) = files::parse_link(&text)
            && self.send_file_link(&room, link, reply_to.as_deref()).await
        {
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
        // Show it straight away (dimmed), then swap in the server's copy.
        let me = room.own_user_id().to_owned();
        let account = self.mgr.accounts().await.into_iter().find(|a| a.id == key.0);
        let local = OwnedEventId::try_from(format!("{}{}", timeline::LOCAL_ECHO, &whisker_core::vault::random_secret()[..12]))
            .expect("valid local event id");
        // People picked from @-suggestions, still named in the text.
        let picked: Vec<(String, OwnedUserId)> = std::mem::take(&mut self.st.lock().unwrap().mentions).into_iter().filter(|(n, _)| text.contains(n.as_str())).collect();
        let item = HistoryItem {
            event_id: local.clone(),
            sender: me,
            sender_name: account.as_ref().and_then(|a| a.profile_name.clone()),
            sender_avatar: account.and_then(|a| a.avatar),
            body: text.clone(),
            ts: matrix_sdk::ruma::MilliSecondsSinceUnixEpoch::now(),
            reply_to: reply_to.clone(),
            media: None,
            edited: false,
            notice: false,
            member: None,
            thread: None,
            pills: picked.iter().map(|(n, u)| (n.clone(), u.to_string())).collect(),

            shield: None,
        };
        self.merge(&key, vec![item]);
        self.ui(|u| u.set_stick_bottom(true));
        self.append_last(&key).await;

        // Picked @-mentions become links (pills in other apps) and are
        // listed in the message so those people get notified.
        match timeline::send_text(&room, &text, reply_to.as_deref(), &picked).await {
            Ok(real) => self.settle_echo(&key, &local, Some(real)).await,
            Err(e) => {
                self.settle_echo(&key, &local, None).await;
                self.status(format!("Not sent: {e:#}"));
                // Give the text back so nothing is lost.
                self.ui(move |u| {
                    if u.get_draft().is_empty() {
                        u.set_draft(s(text));
                    }
                });
            }
        }
    }

    /// Sends the file a pasted link points at as a picture, video or file
    /// message, without uploading it again. False when the link isn't to a
    /// file (or to a message this account can't see), so it goes as text.
    async fn send_file_link(&self, room: &matrix_sdk::Room, link: FileLink, reply_to: Option<&matrix_sdk::ruma::EventId>) -> bool {
        let client = room.client();
        match link {
            FileLink::Media(mxc) => {
                self.status("Sending the file…");
                let file = match files::probe(&client, &mxc).await {
                    Ok(f) => f,
                    Err(e) => {
                        self.status(format!("Couldn't open that file link: {e:#}"));
                        let text = mxc.to_string();
                        self.ui(move |u| {
                            if u.get_draft().is_empty() {
                                u.set_draft(s(text));
                            }
                        });
                        return true;
                    }
                };
                // A picture's or video's size (so it gets its space before
                // it loads), a video's length, and its preview frame.
                let mut details = files::Details::default();
                match file.kind() {
                    MediaKind::Image if file.size.is_none_or(|n| n <= MAX_LINKED_PICTURE_BYTES) => {
                        details.size = timeline::download(&client, &MediaSource::Plain(mxc.clone()), None).await.ok().and_then(|b| {
                            image::ImageReader::new(std::io::Cursor::new(b)).with_guessed_format().ok()?.into_dimensions().ok()
                        });
                    }
                    MediaKind::Video => {
                        self.status("Reading the video…");
                        let posters = self.settings.lock().unwrap().link_posters;
                        if let Some((info, poster)) = files::look_at_video(&client, &mxc, posters).await {
                            details.size = Some((info.width, info.height));
                            details.duration = info.duration;
                            if let Some(p) = poster {
                                self.status("Sending the preview frame…");
                                details.thumbnail = files::upload_poster(&client, &p).await.inspect_err(|e| tracing::warn!("couldn't upload the preview frame: {e:#}")).ok();
                            }
                        }
                    }
                    _ => {}
                }
                match files::send_linked(room, &mxc, &file, &details, reply_to).await {
                    Ok(_) => self.status(""),
                    Err(e) => self.status(format!("Not sent: {e:#}")),
                }
                true
            }
            FileLink::Event(from, event) => {
                let from = match OwnedRoomId::try_from(from.as_str()) {
                    Ok(id) => Some(id),
                    Err(_) => match matrix_sdk::ruma::OwnedRoomAliasId::try_from(from.as_str()) {
                        Ok(alias) => client.resolve_room_alias(&alias).await.ok().map(|r| r.room_id),
                        Err(_) => None,
                    },
                };
                let Some(from) = from.and_then(|id| client.get_room(&id)) else { return false };
                match files::share_file_of(&from, &event, room, reply_to).await {
                    Ok(Some(_)) => {
                        self.status("");
                        true
                    }
                    Ok(None) => false,
                    Err(e) => {
                        tracing::warn!("couldn't share the file of {event}: {e:#}");
                        false
                    }
                }
            }
        }
    }

    /// Replaces a locally shown message with the server's copy (`real`), or
    /// drops it (`None`: sending failed, or the copy arrived through sync first).
    async fn settle_echo(&self, key: &RoomKey, local: &OwnedEventId, real: Option<OwnedEventId>) {
        let keep = {
            let mut st = self.st.lock().unwrap();
            let Some(list) = st.messages.get_mut(key) else { return };
            let synced = real.as_ref().is_some_and(|r| list.iter().any(|m| &m.event_id == r));
            match (real, synced) {
                (Some(real), false) => {
                    if let Some(m) = list.iter_mut().find(|m| &m.event_id == local) {
                        m.event_id = real.clone();
                    }
                    Some(real)
                }
                _ => {
                    list.retain(|m| &m.event_id != local);
                    None
                }
            }
        };
        if self.current().as_ref() != Some(key) {
            return;
        }
        let row = match &keep {
            Some(real) => self.rows(key).await.into_iter().find(|r| &r.item.event_id == real),
            None => None,
        };
        let (px, local) = (self.avatars.clone(), local.to_string());
        self.ui(move |_| {
            ui::with(|m| {
                let Some(i) = ui::index_of(&m.messages, &local) else { return };
                match &row {
                    Some(r) => {
                        let item = m.message(r, &px);
                        m.messages.set_row_data(i, item);
                    }
                    None => {
                        m.messages.remove(i);
                    }
                }
            });
        });
    }

    /// Picks files to send. They wait above the message box, so a caption
    /// can be typed (and a reply picked) first; Enter sends them.
    async fn attach(&self) {
        #[cfg(not(target_os = "android"))]
        {
            if self.current_room().await.is_none() {
                return;
            }
            let picked = tokio::task::spawn_blocking(|| {
                rfd::FileDialog::new().set_title("Send files").pick_files()
            })
            .await
            .ok()
            .flatten();
            let paths = picked.unwrap_or_default();
            if paths.is_empty() {
                return;
            }
            self.st.lock().unwrap().pending_files.extend(paths);
            self.show_pending_files();
            self.ui(|u| u.set_focus_request(u.get_focus_request() + 1));
        }
        #[cfg(target_os = "android")]
        self.status("Uploading isn't available on Android yet.");
    }

    /// Opens the room's whole description in a window of its own, with
    /// its formatting (big rooms keep pages of rules there).
    async fn open_topic(&self) {
        let shown = self.ui.upgrade().map(|u| (u.get_current_room_name().to_string(), u.get_current_room_topic_full().to_string()));
        let (name, mut text) = shown.unwrap_or_default();
        if let Some((_, room)) = self.current_room().await {
            let full = whisker_core::rooms::topic(&room).await;
            if !full.trim().is_empty() {
                text = full;
            }
        }
        self.ui(move |u| {
            let text = if text.trim().is_empty() { "This room has no description.".to_owned() } else { text };
            u.set_topic_title(s(name));
            u.set_topic_blocks(ui::blocks(&text, &[]));
            u.set_topic_text(s(text));
            u.set_topic_open(true);
        });
    }

    /// Saves the picture whose menu was used as a sticker called `name`.
    async fn save_sticker(&self, name: String) {
        let Some(media) = self.st.lock().unwrap().sticker_from.clone() else { return };
        let sticker = match Sticker::from_media(&name, &media) {
            Ok(x) => x,
            Err(e) => {
                let m = format!("{e:#}");
                return self.ui(move |u| u.set_sticker_error(s(m)));
            }
        };
        let Some(vault) = self.spell_vault().await else {
            return self.ui(|u| u.set_sticker_error(s("Unlock an account first.")));
        };
        let mut list: Vec<Sticker> = self.mgr.extra(&vault, stickers::VAULT_KEY).await.unwrap_or_default();
        list.retain(|x| x.name != sticker.name);
        list.push(sticker.clone());
        list.sort_by(|a, b| a.name.cmp(&b.name));
        if let Err(e) = self.mgr.set_extra(&vault, stickers::VAULT_KEY, &list).await {
            let m = format!("Couldn't save it: {e:#}");
            return self.ui(move |u| u.set_sticker_error(s(m)));
        }
        {
            let mut st = self.st.lock().unwrap();
            st.stickers.retain(|(_, x)| x.name != sticker.name);
            st.sticker_pics.remove(&sticker.name);
            st.stickers.push((vault, sticker.clone()));
            st.stickers.sort_by(|a, b| a.1.name.cmp(&b.1.name));
            st.sticker_from = None;
        }
        self.ui(|u| u.set_sticker_save_open(false));
        self.status(format!("Saved as :{}: (type it to send it)", sticker.name));
        self.show_stickers();
    }

    async fn remove_sticker(&self, name: String) {
        let vault = self.st.lock().unwrap().stickers.iter().find(|(_, x)| x.name == name).map(|(v, _)| v.clone());
        if let Some(vault) = vault {
            let mut list: Vec<Sticker> = self.mgr.extra(&vault, stickers::VAULT_KEY).await.unwrap_or_default();
            list.retain(|x| x.name != name);
            if let Err(e) = self.mgr.set_extra(&vault, stickers::VAULT_KEY, &list).await {
                return self.status(format!("Couldn't remove it: {e:#}"));
            }
        }
        self.st.lock().unwrap().stickers.retain(|(_, x)| x.name != name);
        self.show_stickers();
    }

    async fn open_stickers(&self) {
        self.show_stickers();
        self.ui(|u| u.set_stickers_open(true));
    }

    /// Sends a sticker from the picker.
    async fn send_sticker(&self, name: String) {
        let Some((_, room)) = self.current_room().await else { return };
        let sticker = self.st.lock().unwrap().stickers.iter().find(|(_, x)| x.name == name).map(|(_, x)| x.clone());
        self.ui(|u| u.set_stickers_open(false));
        if let Some(sticker) = sticker {
            self.post_sticker(&room, sticker).await;
        }
    }

    /// Posts a sticker, as a reply when one is being written.
    async fn post_sticker(&self, room: &matrix_sdk::Room, sticker: Sticker) {
        let reply_to = self.st.lock().unwrap().reply_to.take();
        self.ui(|u| {
            u.set_reply_to(s(""));
            u.set_stick_bottom(true);
        });
        let sent = match sticker.message() {
            Ok(m) => whisker_core::upload::post_reply(room, m, reply_to.as_deref()).await.map(drop),
            Err(e) => Err(e),
        };
        if let Err(e) = sent {
            self.status(format!("Couldn't send the sticker: {e:#}"));
        }
    }

    /// The picker's rows, loading pictures not seen yet.
    fn show_stickers(&self) {
        let (rows, missing): (Vec<(String, Option<Pixels>)>, bool) = {
            let st = self.st.lock().unwrap();
            let rows: Vec<(String, Option<Pixels>)> = st.stickers.iter().map(|(_, x)| (x.name.clone(), st.sticker_pics.get(&x.name).cloned().flatten())).collect();
            let missing = st.stickers.iter().any(|(_, x)| !st.sticker_pics.contains_key(&x.name));
            (rows, missing)
        };
        if missing {
            self.load_sticker_pics();
        }
        self.ui(move |u| {
            let items: Vec<StickerItem> = rows.iter().map(|(n, p)| StickerItem { name: s(n), picture: p.clone().map(Image::from_rgba8).unwrap_or_default() }).collect();
            u.set_stickers(ModelRc::new(VecModel::from(items)));
        });
    }

    /// Downloads the pictures of stickers not shown yet, then shows them.
    fn load_sticker_pics(&self) {
        let want: Vec<Sticker> = {
            let st = self.st.lock().unwrap();
            st.stickers.iter().map(|(_, x)| x).filter(|x| !st.sticker_pics.contains_key(&x.name)).cloned().collect()
        };
        let Some(account) = self.st.lock().unwrap().account else { return };
        if want.is_empty() {
            return;
        }
        // Marked as loading (no picture yet) so they're asked for once.
        for x in &want {
            self.st.lock().unwrap().sticker_pics.insert(x.name.clone(), None);
        }
        let c = self.clone();
        self.spawn(async move {
            let Some(client) = c.mgr.client(account).await else { return };
            for x in want {
                let Ok(src) = x.source() else { continue };
                let px = match timeline::download(&client, &src, None).await {
                    Ok(b) => tokio::task::spawn_blocking(move || decode(&b, 96, 96, false)).await.ok().flatten(),
                    Err(_) => None,
                };
                c.st.lock().unwrap().sticker_pics.insert(x.name.clone(), px);
            }
            c.show_stickers();
        });
    }

    /// The vault to keep your own additions in: the open account's, else
    /// the last one unlocked.
    async fn spell_vault(&self) -> Option<VaultId> {
        let v = match self.current() {
            Some(key) => self.vault_of(key.0).await,
            None => None,
        };
        v.or_else(|| self.st.lock().unwrap().active_vault.clone())
    }

    async fn cancel_reply(&self) {
        self.st.lock().unwrap().reply_to = None;
    }

    /// Takes a waiting file off the list.
    fn remove_pending_file(&self, index: i32) {
        {
            let mut st = self.st.lock().unwrap();
            if let Ok(i) = usize::try_from(index)
                && i < st.pending_files.len()
            {
                st.pending_files.remove(i);
            }
        }
        self.show_pending_files();
    }

    fn show_pending_files(&self) {
        let names: Vec<SharedString> = self
            .st
            .lock()
            .unwrap()
            .pending_files
            .iter()
            .map(|p| {
                s(p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default())
            })
            .collect();
        self.ui(move |u| u.set_pending_files(ModelRc::new(VecModel::from(names))));
    }

    /// Uploads `paths` (a few at once) and posts them in order: the first as
    /// a reply to `reply_to`, the last with `caption` under it.
    async fn send_files(
        &self,
        room: matrix_sdk::Room,
        paths: Vec<PathBuf>,
        caption: String,
        reply_to: Option<OwnedEventId>,
    ) {
        let opts = self.settings.lock().unwrap().upload.clone();
        let slots = Arc::new(tokio::sync::Semaphore::new(UPLOADS_AT_ONCE));
        let shown = Arc::new(Mutex::new(Uploads {
            total: paths.len(),
            ..Default::default()
        }));
        let count = paths.len();
        let mut jobs = Vec::new();
        for (i, path) in paths.into_iter().enumerate() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let (me, room, opts, slots, shown) = (
                self.clone(),
                room.clone(),
                opts.clone(),
                slots.clone(),
                shown.clone(),
            );
            jobs.push((
                name.clone(),
                tokio::spawn(async move {
                    let _slot = slots.acquire_owned().await;
                    let progress = {
                        let (me, shown) = (me.clone(), shown.clone());
                        move |pct: u8| {
                            let line = {
                                let mut u = shown.lock().unwrap();
                                u.active.insert(i, (name.clone(), pct));
                                u.line()
                            };
                            me.status(line);
                        }
                    };
                    progress(0);
                    let r = timeline::upload_file(&room, &path, &opts, progress).await;
                    shown.lock().unwrap().active.remove(&i);
                    r
                }),
            ));
        }
        let mut failed = Vec::new();
        for (i, (name, job)) in jobs.into_iter().enumerate() {
            let reply = if i == 0 { reply_to.as_deref() } else { None };
            let r = match job.await {
                Ok(Ok(message)) => {
                    let message = if i + 1 == count {
                        whisker_core::upload::with_caption(message, &caption)
                    } else {
                        message
                    };
                    whisker_core::upload::post_reply(&room, message, reply)
                        .await
                        .map(drop)
                }
                Ok(Err(e)) => Err(e),
                Err(e) => Err(anyhow::anyhow!("{e}")),
            };
            let line = {
                let mut u = shown.lock().unwrap();
                u.done += 1;
                u.line()
            };
            match r {
                Ok(()) => self.status(line),
                Err(e) => failed.push(format!("{name}: {e:#}")),
            }
        }
        match failed.as_slice() {
            [] => self.status(""),
            [one] => self.status(format!("Couldn't send {one}")),
            many => self.status(format!(
                "Couldn't send {} files. {}",
                many.len(),
                many.join("; ")
            )),
        }
        // The caption isn't lost when the file carrying it failed.
        if !failed.is_empty() && !caption.is_empty() {
            self.ui(move |u| {
                if u.get_draft().is_empty() {
                    u.set_draft(s(caption));
                }
            });
        }
    }

    fn find(&self, key: &RoomKey, id: &str) -> Option<HistoryItem> {
        self.st.lock().unwrap().messages.get(key)?.iter().find(|m| m.event_id.as_str() == id).cloned()
    }

    async fn message_action(&self, action: String, id: String) {
        let Some((key, room)) = self.current_room().await else { return };
        let Some(m) = self.find(&key, &id) else { return };
        match action.as_str() {
            // Clicking a name replies, but not when that "click" ended a drag selection.
            #[cfg(not(target_os = "android"))]
            "reply" if select::take_just_dragged() => {}
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
                    let draft = format!("{quoted}\n{}", u.get_draft());
                    // Ready to type the answer under the quote.
                    u.set_cursor_at((quoted.len() + 1) as i32);
                    u.set_draft(s(draft));
                    u.set_cursor_request(u.get_cursor_request() + 1);
                });
            }
            "source" => {
                let text = match room.event(&m.event_id, None).await {
                    Ok(ev) => {
                        let pretty = |j: &str| serde_json::from_str::<serde_json::Value>(j).and_then(|v| serde_json::to_string_pretty(&v)).unwrap_or_else(|_| j.to_owned());
                        let shown = pretty(ev.raw().json().get());
                        match ev.encryption_info() {
                            Some(info) => format!("{shown}\n\n// Decrypted. Sent from session {}.", info.sender_device.as_deref().map(|d| d.as_str()).unwrap_or("unknown")),
                            None => shown,
                        }
                    }
                    Err(e) => format!("Couldn't fetch the source: {e}"),
                };
                self.ui(move |u| {
                    u.set_select_title(s("Message source"));
                    u.set_select_text(s(text));
                });
            }
            "edit" => {
                self.st.lock().unwrap().editing = Some(m.event_id.clone());
                // A file's text is its caption, or a stand-in when it has none.
                let body = if m.media.is_some() && !timeline::is_caption(&m.body) { String::new() } else { m.body.clone() };
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
            "retry-media" => self.retry_picture(&key, m),
            "copy-file" => {
                let Some(media) = &m.media else { return };
                let (link, keyed) = files::link_for(&media.source, &room, &m.event_id);
                self.ui(move |u| u.invoke_copy_text(s(link)));
                if keyed {
                    self.status("Copied. The file is encrypted, so the link works for people in this room.");
                }
            }
            "profile" => self.open_profile(&key, m.sender.clone()).await,
            "save" => self.save_media(&key, m).await,
            "save-sticker" => {
                let Some(media) = m.media.clone() else { return };
                let stem = media.name.rsplit_once('.').map_or(media.name.as_str(), |(a, _)| a).to_owned();
                let suggested = stickers::clean_name(&stem).unwrap_or_default();
                self.st.lock().unwrap().sticker_from = Some(media);
                self.ui(move |u| {
                    u.set_sticker_name(s(suggested));
                    u.set_sticker_error(s(""));
                    u.set_sticker_save_open(true);
                });
            }
            "play" => self.play_media(&key, m).await,
            "view" => self.view_image(&key, m).await,
            "forward" => self.open_forward(key, m.event_id).await,
            "thread" => self.open_thread(m.thread.clone().unwrap_or(m.event_id)).await,
            a if a.starts_with("react:") => self.toggle_reaction(&key, &room, &m.event_id, &a[6..]).await,
            _ => {}
        }
    }

    /// Adds our `emoji` reaction to a message, or takes it back if it's there.
    async fn toggle_reaction(&self, key: &RoomKey, room: &matrix_sdk::Room, target: &OwnedEventId, emoji: &str) {
        let me = room.own_user_id().to_owned();
        let ours = self.st.lock().unwrap().reactions.get(key).and_then(|l| l.iter().find(|r| &r.target == target && r.key == emoji && r.sender == me).map(|r| r.event_id.clone()));
        match ours {
            Some(id) => {
                if let Some(l) = self.st.lock().unwrap().reactions.get_mut(key) {
                    l.retain(|r| r.event_id != id);
                }
                self.update_message(key, target);
                if !id.as_str().starts_with(timeline::LOCAL_ECHO)
                    && let Err(e) = room.redact(&id, None, None).await
                {
                    self.status(format!("Couldn't remove the reaction: {e}"));
                }
            }
            None => {
                // Show it straight away; swap in the real id once sent.
                let Ok(local) = OwnedEventId::try_from(format!("{}{}", timeline::LOCAL_ECHO, &whisker_core::vault::random_secret()[..12])) else { return };
                let r = timeline::Reaction { event_id: local, target: target.clone(), key: emoji.to_owned(), sender: me };
                self.merge_reactions(key, vec![r.clone()]);
                self.update_message(key, target);
                let sent = timeline::react(room, target, emoji).await;
                let mut st = self.st.lock().unwrap();
                let list = st.reactions.entry(key.clone()).or_default();
                match sent {
                    Ok(id) => {
                        if list.iter().any(|x| x.event_id == id) {
                            list.retain(|x| x.event_id != r.event_id);
                        } else if let Some(x) = list.iter_mut().find(|x| x.event_id == r.event_id) {
                            x.event_id = id;
                        }
                    }
                    Err(e) => {
                        list.retain(|x| x.event_id != r.event_id);
                        drop(st);
                        self.status(format!("Couldn't react: {e:#}"));
                    }
                }
                self.update_message(key, target);
            }
        }
    }

    /// Opens "Forward to…" for a message, listing the account's rooms.
    async fn open_forward(&self, key: RoomKey, event_id: OwnedEventId) {
        self.st.lock().unwrap().forwarding = Some((key, event_id));
        self.ui(|u| {
            u.set_forward_query(s(""));
            u.set_forward_status(s(""));
            u.set_forward_open(true);
        });
        self.forward_search(String::new()).await;
    }

    async fn forward_search(&self, q: String) {
        let Some(((account, _), _)) = self.st.lock().unwrap().forwarding.clone() else { return };
        self.room_choices(account, q).await;
    }

    /// Fills the room list the Forward and Share dialogs pick from: the
    /// account's rooms whose name has `q`. Returns how many there are.
    async fn room_choices(&self, account: AccountId, q: String) -> usize {
        let q = q.to_lowercase();
        let rooms: Vec<_> = self.mgr.rooms(account).await.into_iter().filter(|r| !r.invite && !r.is_space && (q.is_empty() || r.name.to_lowercase().contains(&q))).collect();
        let n = rooms.len();
        self.fetch_avatars(account, rooms.iter().filter_map(|r| r.avatar.clone()));
        let px = self.avatars.clone();
        self.ui(move |_| {
            ui::with(|m| {
                let items: Vec<ForwardItem> = rooms
                    .iter()
                    .map(|r| {
                        let (initial, hue) = initial_hue(&r.name);
                        ForwardItem { id: s(r.room_id.as_str()), name: s(&r.name), initial, hue, avatar: m.avatar(&r.avatar, &px), sent: false }
                    })
                    .collect();
                m.forward.set_vec(items);
            });
        });
        n
    }

    async fn forward_to(&self, target: String) {
        let Some(((account, from_id), event_id)) = self.st.lock().unwrap().forwarding.clone() else { return };
        let Some(client) = self.mgr.client(account).await else { return };
        let (Some(from), Some(to)) = (client.get_room(&from_id), OwnedRoomId::try_from(target.as_str()).ok().and_then(|id| client.get_room(&id))) else { return };
        // An edited text message goes as its newest text.
        let edited = self.find(&(account, from_id.clone()), event_id.as_str()).filter(|m| m.edited && m.media.is_none()).map(|m| m.body);
        let mark = |sent: bool, id: String| {
            move |_: &AppWindow| {
                ui::with(|m| {
                    if let Some(i) = (0..m.forward.row_count()).find(|&i| m.forward.row_data(i).is_some_and(|r| r.id == id.as_str())) {
                        let mut r = m.forward.row_data(i).unwrap();
                        r.sent = sent;
                        m.forward.set_row_data(i, r);
                    }
                });
            }
        };
        self.ui(mark(true, target.clone()));
        match timeline::forward(&from, &event_id, &to, edited.as_deref()).await {
            Ok(_) => {
                let name = timeline::room_name(&to).await;
                self.ui(move |u| u.set_forward_status(s(format!("Forwarded to {name}."))));
            }
            Err(e) => {
                let m = format!("Couldn't forward: {e:#}");
                self.ui(mark(false, target));
                self.ui(move |u| u.set_forward_status(s(m)));
            }
        }
    }

    // ---- spaces ----

    /// Fills the space page: the rooms the space lists, joined or not.
    async fn show_space(&self, key: &RoomKey, space: &matrix_sdk::Room) {
        let cached = self.st.lock().unwrap().space_rooms.get(key).filter(|(t, _)| t.elapsed() < Duration::from_secs(60)).map(|(_, l)| l.clone());
        let list = match cached {
            Some(l) => l,
            None => {
                self.ui(|u| {
                    u.set_space_loading(true);
                    u.set_space_rooms(ModelRc::default());
                });
                let l = timeline::space_rooms(space).await;
                self.st.lock().unwrap().space_rooms.insert(key.clone(), (Instant::now(), l.clone()));
                l
            }
        };
        if self.current().as_ref() != Some(key) {
            return;
        }
        self.fetch_avatars(key.0, list.iter().filter_map(|r| r.avatar.clone()));
        let client = space.client();
        let joined: Vec<bool> = list.iter().map(|r| client.get_room(&r.room_id).is_some_and(|x| x.state() == matrix_sdk::RoomState::Joined)).collect();
        let px = self.avatars.clone();
        self.ui(move |u| {
            let items: Vec<SpaceRoomItem> = ui::with(|m| {
                list.iter()
                    .zip(joined)
                    .map(|(r, joined)| {
                        let (initial, hue) = initial_hue(&r.name);
                        SpaceRoomItem {
                            id: s(r.room_id.as_str()),
                            name: s(&r.name),
                            topic: s(first_line(r.topic.as_deref().unwrap_or_default())),
                            members: s(if r.members == 1 {
                                "1 member".into()
                            } else {
                                format!("{} members", r.members)
                            }),
                            initial,
                            hue,
                            avatar: m.avatar(&r.avatar, &px),
                            joined,
                            space: r.is_space,
                            busy: false,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
            u.set_space_rooms(ModelRc::new(VecModel::from(items)));
            u.set_space_loading(false);
        });
    }

    /// Joins a room listed on the open space's page, then opens it.
    async fn space_join(&self, id: String) {
        let Some((key, space)) = self.current_room().await else { return };
        let Some(r) = self.st.lock().unwrap().space_rooms.get(&key).and_then(|(_, l)| l.iter().find(|r| r.room_id.as_str() == id).cloned()) else { return };
        let busy = |on: bool, id: String| {
            move |u: &AppWindow| {
                let rows = u.get_space_rooms();
                if let Some(i) = (0..rows.row_count()).find(|&i| rows.row_data(i).is_some_and(|x| x.id == id.as_str())) {
                    let mut x = rows.row_data(i).unwrap();
                    x.busy = on;
                    rows.set_row_data(i, x);
                }
            }
        };
        self.ui(busy(true, id.clone()));
        match timeline::join_listed(&space.client(), &r).await {
            Ok(room) => {
                self.st.lock().unwrap().space_rooms.remove(&key);
                self.ui(move |u| {
                    u.set_current_room(s(room.room_id().as_str()));
                    u.set_stick_bottom(true);
                });
                self.select_room(id).await;
            }
            Err(e) => {
                let m = format!("Couldn't join {}: {e:#}", r.name);
                self.ui(busy(false, id));
                self.ui(move |u| u.set_space_status(s(m)));
            }
        }
    }

    // ---- sharing from other apps ----

    /// Takes files or text shared from another app (any thread). Opens the
    /// Share dialog, or waits for the vault to be unlocked.
    fn receive_share(&self, items: Vec<share::Shared>) {
        let c = self.clone();
        self.spawn(async move {
            c.ui(|u| {
                let _ = u.show();
            });
            if items.is_empty() {
                return;
            }
            let (what, unlocked) = {
                let mut st = c.st.lock().unwrap();
                st.sharing.extend(items);
                (share::summary(&st.sharing), st.active_vault.is_some())
            };
            c.ui(move |u| u.set_share_what(s(what)));
            if unlocked {
                c.open_share().await;
            }
        });
    }

    /// The Share dialog: which account sends it (skipped when there's only one).
    async fn open_share(&self) {
        let accounts = self.mgr.accounts().await;
        for a in &accounts {
            if let Some(m) = &a.avatar {
                self.fetch_avatars(a.id, [m.clone()]);
            }
        }
        let only = (accounts.len() == 1).then(|| accounts[0].id);
        let px = self.avatars.clone();
        self.ui(move |u| {
            let items: Vec<AccountItem> = ui::with(|m| {
                accounts
                    .iter()
                    .map(|a| {
                        let (initial, hue) = initial_hue(&a.display_name);
                        AccountItem {
                            id: s(a.id.to_string()),
                            name: s(&a.display_name),
                            user_id: s(&a.user_id),
                            tags: s(""),
                            is_agent: a.kind == AccountKind::Agent,
                            initial,
                            hue,
                            avatar: m.avatar(&a.avatar, &px),
                            unread: 0,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
            u.set_share_accounts(ModelRc::new(VecModel::from(items)));
            u.set_share_step(0);
            u.set_share_query(s(""));
            u.set_share_status(s(""));
            u.set_share_busy(false);
            u.set_share_open(true);
        });
        if let Some(id) = only {
            self.share_pick_account(id.to_string()).await;
        }
    }

    async fn share_pick_account(&self, id: String) {
        let Ok(id) = id.parse::<AccountId>() else { return };
        self.st.lock().unwrap().share_account = Some(id);
        let name = self.mgr.accounts().await.into_iter().find(|a| a.id == id).map(|a| a.display_name).unwrap_or_default();
        self.ui(move |u| {
            ui::with(|m| m.forward.set_vec(Vec::new()));
            u.set_share_account_name(s(name));
            u.set_share_query(s(""));
            u.set_share_step(1);
        });
        // Right after unlock the account may still be loading its rooms.
        for _ in 0..30 {
            if self.st.lock().unwrap().share_account != Some(id) || self.room_choices(id, String::new()).await > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn share_search(&self, q: String) {
        let Some(account) = self.st.lock().unwrap().share_account else { return };
        self.room_choices(account, q).await;
    }

    fn share_back(&self) {
        self.st.lock().unwrap().share_account = None;
        self.ui(|u| {
            ui::with(|m| m.forward.set_vec(Vec::new()));
            u.set_share_status(s(""));
            u.set_share_step(0);
        });
    }

    fn share_cancel(&self) {
        {
            let mut st = self.st.lock().unwrap();
            st.sharing.clear();
            st.share_account = None;
        }
        self.ui(|u| {
            u.set_share_open(false);
            u.set_share_what(s(""));
        });
    }

    /// Sends everything shared to the room, then opens it.
    async fn share_to(&self, target: String) {
        let (Some(account), items) = ({
            let st = self.st.lock().unwrap();
            (st.share_account, st.sharing.clone())
        }) else {
            return;
        };
        let Some(client) = self.mgr.client(account).await else { return };
        let Some(room) = OwnedRoomId::try_from(target.as_str()).ok().and_then(|id| client.get_room(&id)) else { return };
        let mark = |sending: bool, id: String| {
            move |u: &AppWindow| {
                u.set_share_busy(sending);
                ui::with(|m| {
                    if let Some(i) = (0..m.forward.row_count()).find(|&i| m.forward.row_data(i).is_some_and(|r| r.id == id.as_str())) {
                        let mut r = m.forward.row_data(i).unwrap();
                        r.sent = sending;
                        m.forward.set_row_data(i, r);
                    }
                });
            }
        };
        self.ui(mark(true, target.clone()));
        let total = items.len();
        let mut too_big = Vec::new();
        for (i, item) in items.into_iter().enumerate() {
            if total > 1 {
                let m = format!("Sending {} of {total}…", i + 1);
                self.ui(move |u| u.set_share_status(s(m)));
            }
            let sent = match item {
                share::Shared::File { name, data } => {
                    let opts = self.settings.lock().unwrap().upload.clone();
                    timeline::send_bytes(&room, &name, data, &opts)
                        .await
                        .map_err(|e| (name, e))
                }
                share::Shared::Text(t) => timeline::send_text(&room, &t, None, &[])
                    .await
                    .map(drop)
                    .map_err(|e| ("the text".into(), e)),
                share::Shared::TooBig(name) => {
                    too_big.push(name);
                    Ok(())
                }
            };
            if let Err((name, e)) = sent {
                // What went already is done; a retry sends only the rest.
                self.st.lock().unwrap().sharing.drain(..i);
                let m = format!("Couldn't send {name}: {e:#}");
                self.ui(mark(false, target));
                self.ui(move |u| u.set_share_status(s(m)));
                return;
            }
        }
        let more = {
            let mut st = self.st.lock().unwrap();
            st.sharing.drain(..total);
            st.share_account = None;
            st.account_chosen = true;
            st.account = Some(account);
            st.room = None;
            !st.sharing.is_empty()
        };
        let room_id = target.clone();
        self.ui(move |u| {
            u.set_share_busy(false);
            u.set_share_open(false);
            u.set_share_what(s(""));
            u.set_exploring(false);
            u.set_pane(2);
            u.set_current_room(s(&room_id));
        });
        self.select_room(target).await;
        self.refresh_accounts_only().await;
        if !too_big.is_empty() {
            self.status(format!("Not sent (over 100 MB): {}", too_big.join(", ")));
        }
        // More came in while this was sending.
        if more {
            let what = share::summary(&self.st.lock().unwrap().sharing);
            self.ui(move |u| u.set_share_what(s(what)));
            self.open_share().await;
        }
    }

    /// The emoji picker's grid for `q`, eight to a row.
    fn emoji_search(&self, q: String) {
        let found = emoji::search(&q);
        self.ui(move |u| u.set_emoji_rows(emoji_rows(&found)));
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
                let bytes = timeline::download_in(&client, &key.1, &media.source, None).await?;
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

    /// Opens a video or audio file in the system's player. The file goes
    /// to the RAM folder (wiped when you lock), not the disk.
    async fn play_media(&self, key: &RoomKey, m: HistoryItem) {
        let Some(media) = m.media else { return };
        let Some(client) = self.mgr.client(key.0).await else { return };
        self.status("Loading…");
        #[cfg(target_os = "linux")]
        if media.kind == MediaKind::Video && player::available() {
            player::stop(&self.ui);
            let id = m.event_id.to_string();
            self.ui(move |u| {
                let p = u.global::<Player>();
                p.set_id(s(id));
                p.set_loading(true);
            });
        }
        let dir = media_dir(self.mgr.data_dir());
        let ext = std::path::Path::new(&media.name).extension().and_then(|e| e.to_str()).filter(|e| e.len() <= 5 && e.chars().all(char::is_alphanumeric)).unwrap_or("bin").to_owned();
        let path = dir.join(format!("{}.{ext}", &whisker_core::vault::random_secret()[..16]));
        let r = async {
            let bytes = timeline::download_in(&client, &key.1, &media.source, None).await?;
            whisker_core::stores::make_private_dir(&dir)?;
            tokio::fs::write(&path, bytes).await?;
            whisker_core::vault::owner_only(&path, false);
            anyhow::Ok(())
        }
        .await;
        match r {
            Ok(()) => {
                self.status("");
                // Videos play in the message itself where ffmpeg can show them.
                #[cfg(target_os = "linux")]
                if media.kind == MediaKind::Video && player::available() {
                    let (ui, id, path) = (self.ui.clone(), m.event_id.to_string(), path.clone());
                    let played = tokio::task::spawn_blocking(move || player::play(ui, id, path)).await.unwrap_or(false);
                    if played {
                        return;
                    }
                    self.ui(|u| u.global::<Player>().set_loading(false));
                }
                play_file(&path, media.kind == MediaKind::Video);
            }
            Err(e) => {
                #[cfg(target_os = "linux")]
                self.ui(|u| u.global::<Player>().set_loading(false));
                self.status(format!("Couldn't open it: {e:#}"));
            }
        }
    }

    async fn view_image(&self, key: &RoomKey, m: HistoryItem) {
        let Some(media) = m.media else { return };
        let id = m.event_id.to_string();
        self.ui(move |u| {
            u.set_viewer_id(s(id));
            u.set_viewer_image(Image::default());
            u.set_viewer_frames(ModelRc::default());
            u.set_viewer_open(true);
        });
        let Some(client) = self.mgr.client(key.0).await else { return };
        let decoded = match timeline::download_in(&client, &key.1, &media.source, None).await {
            Ok(b) => tokio::task::spawn_blocking(move || (decode(&b, 2560, 2560, false), ui::decode_frames(&b, 1600, 1200))).await.unwrap_or((None, None)),
            Err(_) => (None, None),
        };
        match decoded.0 {
            Some(px) => self.ui(move |u| {
                u.set_viewer_image(Image::from_rgba8(px));
                // GIFs and animated WebP play (and loop) in the viewer too.
                let (frames, ms) = decoded.1.map(|f| (f.0.iter().map(|p| Image::from_rgba8(p.clone())).collect::<Vec<_>>(), f.1)).unwrap_or_default();
                u.set_viewer_frames(ModelRc::new(VecModel::from(frames)));
                u.set_viewer_frame_ms(ms as i32);
            }),
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
        let (role, can_kick, can_ban, (level_now, can_set_level, max_level)) =
            match (room.power_levels().await, &me) {
                (Ok(pl), Some(me)) => {
                    use matrix_sdk::ruma::events::room::power_levels::UserPowerLevel;
                    let level: i64 = match pl.for_user(&user) {
                        UserPowerLevel::Int(i) => i.into(),
                        _ => i64::MAX,
                    };
                    // A creator (who outranks everyone) gives up to the usual admin level.
                    let mine: i64 = match pl.for_user(me) {
                        UserPowerLevel::Int(i) => i.into(),
                        _ => 100,
                    };
                    let levels = (
                        if level == i64::MAX {
                            -1
                        } else {
                            level.clamp(-1_000_000, 1_000_000) as i32
                        },
                        pl.user_can_change_user_power_level(me, &user),
                        mine.clamp(-1_000_000, 1_000_000) as i32,
                    );
                    let role = match level {
                        i64::MAX => "Creator".to_owned(),
                        100.. => "Admin".to_owned(),
                        50..=99 => "Moderator".to_owned(),
                        1..=49 => format!("Power {level}"),
                        _ => String::new(),
                    };
                    (
                        role,
                        pl.user_can_kick_user(me, &user),
                        pl.user_can_ban_user(me, &user),
                        levels,
                    )
                }
                _ => (String::new(), false, false, (0, false, 0)),
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
            Some(m) => match timeline::thumbnail(&client, m, 256).await {
                Ok(b) => {
                    let (big, small) = tokio::task::spawn_blocking(move || (decode(&b, 256, 256, true), decode(&b, AVATAR_PX, AVATAR_PX, true))).await.unwrap_or((None, None));
                    // The same picture next to their messages, if it hadn't loaded there.
                    if let Some(small) = small {
                        let mut cache = self.avatars.lock().unwrap();
                        if !matches!(cache.get(m), Some(Some(_))) {
                            cache.insert(m.clone(), Some(small));
                            drop(cache);
                            self.loading.lock().unwrap().remove(m.as_str());
                            let c = self.clone();
                            self.spawn(async move { c.show_messages().await });
                        }
                    }
                    big
                }
                Err(_) => None,
            },
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
            match client.encryption().request_user_identity(&user).await {
                Ok(Some(i)) if i.has_verification_violation() => 3,
                Ok(Some(i)) if i.is_verified() => 2,
                Ok(Some(_)) => 1,
                _ => 0,
            }
        };
        // Their logins, verified ones first.
        let mut sessions: Vec<(String, String, bool)> = match client.encryption().get_user_devices(&user).await {
            Ok(list) => list.devices().map(|d| (d.display_name().unwrap_or("Unnamed session").to_owned(), d.device_id().to_string(), d.is_verified())).collect(),
            Err(_) => Vec::new(),
        };
        sessions.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
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
                level: level_now,
                can_set_level,
                max_level,
                has_dm,
                verified,
                sessions: ModelRc::new(VecModel::from(
                    sessions.iter().map(|(name, id, v)| SessionItem { name: s(name), id: s(id), verified: *v }).collect::<Vec<_>>(),
                )),
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
            a if a.starts_with("level:") => {
                let Ok(level) = a["level:".len()..].parse::<i64>() else {
                    return;
                };
                let act = ModAction::SetPowerLevel {
                    user: user.clone(),
                    level,
                };
                match act.apply(&room).await {
                    Ok(()) => {
                        self.status(format!("Done: {act}"));
                        // Show the new role (and what may still be changed).
                        self.open_profile(&key, user).await;
                    }
                    Err(e) => self.status(format!("Couldn't change the role: {e}")),
                }
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

    fn set_notify_level(&self, level: i32) {
        let mut st = self.settings.lock().unwrap();
        st.notify_level = level.clamp(0, 3);
        let _ = st.save(self.mgr.data_dir());
        if let Some(u) = self.ui.upgrade() {
            u.global::<Prefs>().set_notify_level(st.notify_level);
        }
    }

    fn set_menu_entry(&self, key: String, on: bool) {
        let mut st = self.settings.lock().unwrap();
        let (mark, set) = if MENU_OFF_BY_DEFAULT.contains(&key.as_str()) { (format!("+{key}"), on) } else { (key, !on) };
        if set {
            st.menu_hidden.insert(mark);
        } else {
            st.menu_hidden.remove(&mark);
        }
        let _ = st.save(self.mgr.data_dir());
        if let Some(u) = self.ui.upgrade() {
            apply_menu(&u, &st.menu_hidden);
        }
    }

    fn set_clear_command(&self, cmd: String) {
        let mut st = self.settings.lock().unwrap();
        st.clear_command = cmd.trim().to_owned();
        let _ = st.save(self.mgr.data_dir());
    }

    async fn clear_messages(&self, n: i32) {
        let Some((_, room)) = self.current_room().await else { return };
        self.status("Clearing messages…");
        let me = self.clone();
        let limit = usize::try_from(n).ok().filter(|n| *n > 0);
        match timeline::clear_room(&room, limit, move |done| me.status(format!("Clearing messages… {done} deleted"))).await {
            Ok((done, 0)) => self.status(format!("Deleted {done} messages.")),
            Ok((done, failed)) => self.status(format!("Deleted {done} messages; {failed} couldn't be deleted.")),
            Err(e) => self.status(format!("Couldn't clear messages: {e:#}")),
        }
    }

    /// One upload setting changed (`key` as the Settings page names it).
    fn set_upload(&self, key: &str, v: i32) {
        use whisker_core::upload::{Convert, ThumbFormat};
        let mut st = self.settings.lock().unwrap();
        let u = &mut st.upload;
        let on = v != 0;
        match key {
            "strip" => u.strip_metadata = on,
            "hash" => u.hash_names = on,
            "convert" => {
                u.convert = [Convert::Keep, Convert::Jpeg, Convert::Png]
                    .get(v as usize)
                    .copied()
                    .unwrap_or_default()
            }
            "quality" => u.quality = v.clamp(1, 100) as u8,
            "thumbs" => u.thumbnails = on,
            "thumb-format" => {
                u.thumb_format = if v == 1 {
                    ThumbFormat::Png
                } else {
                    ThumbFormat::Jpeg
                }
            }
            "thumb-quality" => u.thumb_quality = v.clamp(1, 100) as u8,
            "encrypt" => u.always_encrypt = on,
            _ => return,
        }
        let _ = st.save(self.mgr.data_dir());
        let u = st.upload.clone();
        drop(st);
        self.ui(move |ui| show_upload_prefs(&ui.global::<Prefs>(), &u));
    }

    fn set_ask_room_servers(&self, on: bool) {
        let mut st = self.settings.lock().unwrap();
        st.ask_room_servers = on;
        let _ = st.save(self.mgr.data_dir());
        whisker_core::media_fetch::set_ask_room_servers(on);
        if let Some(u) = self.ui.upgrade() {
            u.global::<Prefs>().set_ask_room_servers(on);
        }
    }

    fn set_ask_file_servers(&self, on: bool) {
        let mut st = self.settings.lock().unwrap();
        st.ask_file_servers = on;
        let _ = st.save(self.mgr.data_dir());
        whisker_core::media_fetch::set_ask_file_servers(on);
        if let Some(u) = self.ui.upgrade() {
            u.global::<Prefs>().set_ask_file_servers(on);
        }
    }

    fn set_link_posters(&self, on: bool) {
        let mut st = self.settings.lock().unwrap();
        st.link_posters = on;
        let _ = st.save(self.mgr.data_dir());
        if let Some(u) = self.ui.upgrade() {
            u.global::<Prefs>().set_link_posters(on);
        }
    }

    fn set_spell_check(&self, on: bool) {
        let mut st = self.settings.lock().unwrap();
        st.spell_check = on;
        let _ = st.save(self.mgr.data_dir());
        if let Some(u) = self.ui.upgrade() {
            u.global::<Prefs>().set_spell_check(on);
            #[cfg(not(target_os = "android"))]
            spell_marks::set_on(&u, on);
        }
    }

    /// Keeps a word added to the dictionary, in the open account's vault.
    async fn save_spell_word(&self, word: String) {
        let vault = match self.current() {
            Some(key) => self.vault_of(key.0).await,
            None => None,
        }
        .or_else(|| self.st.lock().unwrap().active_vault.clone());
        let Some(vault) = vault else { return };
        let mut words: Vec<String> = self
            .mgr
            .extra(&vault, spell::VAULT_KEY)
            .await
            .unwrap_or_default();
        let word = word.to_lowercase();
        if !words.contains(&word) {
            words.push(word);
            words.sort();
            if let Err(e) = self.mgr.set_extra(&vault, spell::VAULT_KEY, &words).await {
                self.status(format!("Couldn't save the word: {e:#}"));
            }
        }
    }

    fn set_autocorrect(&self, on: bool) {
        let mut st = self.settings.lock().unwrap();
        st.autocorrect = on;
        let _ = st.save(self.mgr.data_dir());
        if let Some(u) = self.ui.upgrade() {
            u.global::<Prefs>().set_autocorrect(on);
        }
    }

    /// Runs on the UI thread for every keystroke in the composer, so it sees
    /// them in order. Returns the draft to show instead of `text`: a typo
    /// just finished, fixed; or, on Backspace straight after a fix, the word
    /// as typed (and that word isn't fixed again).
    fn fix_typing(&self, text: &str) -> Option<String> {
        let mut st = self.st.lock().unwrap();
        let prev = std::mem::replace(&mut st.typed, text.to_owned());
        if let Some((after, before, word)) = st.autocorrected.take()
            && after.char_indices().last().is_some_and(|(i, _)| after[..i] == *text)
        {
            st.no_fix.insert(word.to_lowercase());
            st.typed = before.clone();
            return Some(before);
        }
        drop(st);
        let new = self.autocorrect(&prev, text)?;
        let mut st = self.st.lock().unwrap();
        st.autocorrected = Some((new.clone(), prev.clone(), last_word(&prev).to_owned()));
        st.typed = new.clone();
        Some(new)
    }

    /// The draft with a just-finished typo fixed, if `text` is `prev` with
    /// one word-ending character typed at the end.
    fn autocorrect(&self, prev: &str, text: &str) -> Option<String> {
        if !self.settings.lock().unwrap().autocorrect {
            return None;
        }
        let end = text.strip_prefix(prev)?;
        let mut cs = end.chars();
        let (c, None) = (cs.next()?, cs.next()) else { return None };
        if !autocorrect::ends_word(c) || prev.matches('`').count() % 2 == 1 {
            return None;
        }
        let word = last_word(prev);
        if self.st.lock().unwrap().no_fix.contains(&word.to_lowercase()) {
            return None;
        }
        let fixed = autocorrect::fix(word)?;
        Some(format!("{}{fixed}{c}", &prev[..prev.len() - word.len()]))
    }

    fn set_send_typing(&self, on: bool) {
        let mut st = self.settings.lock().unwrap();
        st.send_typing = on;
        let _ = st.save(self.mgr.data_dir());
        if let Some(u) = self.ui.upgrade() {
            u.global::<Prefs>().set_send_typing(on);
        }
    }

    /// Shows a desktop notification for a new message when it should:
    /// not our own (or our other accounts'), not old, not in the room you're
    /// looking at, not for an AI account, and following the room's setting
    /// (all messages, mentions only, or muted; by default DMs and mentions).
    async fn maybe_notify(&self, account: AccountId, room_id: OwnedRoomId, sender: OwnedUserId, sender_name: Option<String>, body: String, ts: MilliSecondsSinceUnixEpoch) {
        use matrix_sdk::notification_settings::RoomNotificationMode as Mode;
        let detail = notify::Detail::from_index(self.settings.lock().unwrap().notify_level);
        if detail == notify::Detail::Off || !self.st.lock().unwrap().synced.contains(&account) {
            return;
        }
        let age = MilliSecondsSinceUnixEpoch::now().get().saturating_sub(ts.get());
        if age > UInt::from(120_000u32) {
            return;
        }
        let key = (account, room_id.clone());
        let looking = {
            let st = self.st.lock().unwrap();
            st.account == Some(account) && st.room.as_ref() == Some(&room_id) && st.last_activity.is_some_and(|t| t.elapsed() < Duration::from_secs(60))
        };
        let accounts = self.mgr.accounts().await;
        let me = accounts.iter().find(|a| a.id == account);
        if looking || me.is_none_or(|a| a.kind == AccountKind::Agent) || accounts.iter().any(|a| a.user_id == sender.as_str()) {
            return;
        }
        let Some(client) = self.mgr.client(account).await else { return };
        let Some(room) = client.get_room(&room_id) else { return };
        let direct = room.is_direct().await.unwrap_or(false) || room.joined_members_count() <= 2;
        let mut names = vec![sender_of_me(&client), me.map(|a| a.display_name.clone()).unwrap_or_default()];
        names.retain(|n| n.chars().count() > 1);
        let mentioned = whisker_agent::memory::mentioned(&body, &names);
        let mode = room.notification_mode().await;
        let wanted = match mode {
            Some(Mode::Mute) => false,
            Some(Mode::AllMessages) => true,
            Some(Mode::MentionsAndKeywordsOnly) => mentioned,
            None => direct || mentioned,
        };
        if !wanted {
            return;
        }
        let who = sender_name.unwrap_or_else(|| sender.localpart().to_owned());
        let room_name = timeline::room_name(&room).await;
        let text = if timeline::is_undecryptable(&body) { "Encrypted message".to_owned() } else { body };
        let Some((summary, text)) = notify::content(detail, &who, &room_name, direct, &text) else { return };
        // One notification per room: a newer message replaces it.
        let replaces = self.st.lock().unwrap().notified.get(&key).filter(|(_, t)| t.elapsed() < Duration::from_secs(300)).map_or(0, |n| n.0);
        if let Ok(Some(id)) = tokio::task::spawn_blocking(move || notify::show(&summary, &text, replaces)).await {
            self.st.lock().unwrap().notified.insert(key, (id, Instant::now()));
        }
    }

    /// "Bob is typing…" under the open room.
    async fn show_typing(&self, key: &RoomKey) {
        let users = self.st.lock().unwrap().typing.get(key).cloned().unwrap_or_default();
        let room = self.mgr.client(key.0).await.and_then(|c| c.get_room(&key.1));
        let mut names = Vec::new();
        for u in &users {
            let n = match &room {
                Some(r) => timeline::member_profile(r, u).await.0,
                None => None,
            };
            names.push(n.unwrap_or_else(|| u.localpart().to_owned()));
        }
        let text = match names.as_slice() {
            [] => String::new(),
            [a] => format!("{a} is typing…"),
            [a, b] => format!("{a} and {b} are typing…"),
            [a, b, rest @ ..] => format!("{a}, {b} and {} more are typing…", rest.len()),
        };
        self.ui(move |u| u.set_typing_text(s(text)));
    }

    /// The draft changed: offer @-mentions or :emoji: for the word being
    /// typed, and say we're typing if that's on.
    async fn draft_edited(&self, text: String) {
        // A finished `:name:` becomes its emoji straight away.
        let word = last_word(&text);
        if let Some(e) = word
            .strip_prefix(':')
            .and_then(|w| w.strip_suffix(':'))
            .filter(|n| !n.is_empty() && !self.st.lock().unwrap().stickers.iter().any(|(_, x)| x.name == *n))
            .and_then(emoji::shortcode)
        {
            let new = format!("{}{e}", &text[..text.len() - word.len()]);
            self.st.lock().unwrap().draft = new.clone();
            self.ui(move |u| {
                u.set_suggestions(ModelRc::default());
                u.set_draft(s(&new));
                u.set_cursor_at(new.len() as i32);
                u.set_cursor_request(u.get_cursor_request() + 1);
            });
            return;
        }
        self.st.lock().unwrap().draft = text.clone();
        // (label, detail, insert, kind); built into Slint rows on the UI thread.
        let mut out: Vec<(String, String, String, i32)> = Vec::new();
        let mut pics: HashMap<String, Pixels> = HashMap::new();
        if let Some(q) = word.strip_prefix('@').filter(|q| !q.is_empty()) {
            if let Some((key, room)) = self.current_room().await {
                for (user, name) in best_members(&self.members(&key, &room).await, q).into_iter().take(6) {
                    out.push((name.clone(), user.to_string(), name, 0));
                }
            }
        } else if let Some(q) = word.strip_prefix(':').filter(|q| !q.is_empty() && !q.contains(':')) {
            let found: Vec<Sticker> = {
                let st = self.st.lock().unwrap();
                let list: Vec<Sticker> = st.stickers.iter().map(|(_, x)| x.clone()).collect();
                let found: Vec<Sticker> = stickers::suggest(&list, q).into_iter().take(4).cloned().collect();
                for x in &found {
                    if let Some(Some(p)) = st.sticker_pics.get(&x.name) {
                        pics.insert(x.name.clone(), p.clone());
                    }
                }
                found
            };
            if found.iter().any(|x| !pics.contains_key(&x.name)) {
                self.load_sticker_pics();
            }
            for x in &found {
                let tag = format!(":{}:", x.name);
                out.push((x.name.clone(), format!("{tag} · sticker"), tag, 2));
            }
            for e in emoji::suggest(q).into_iter().take(8) {
                out.push((e.to_owned(), format!(":{}:", emoji::name_of(e)), e.to_owned(), 1));
            }
        }
        self.ui(move |u| {
            let rows: Vec<Suggestion> = out
                .iter()
                .map(|(label, detail, insert, kind)| Suggestion {
                    picture: match kind {
                        1 => ui::emoji_image(label),
                        2 => pics.get(label).map(|p| Image::from_rgba8(p.clone())).unwrap_or_default(),
                        _ => Image::default(),
                    },
                    label: s(label),
                    detail: s(detail),
                    insert: s(insert),
                    kind: *kind,
                })
                .collect();
            u.set_suggestions(ModelRc::new(VecModel::from(rows)));
        });
        self.composing(text.is_empty()).await;
    }

    /// Puts a picked suggestion in place of the word being typed.
    fn pick_suggestion(&self, insert: String, user: String) {
        let new = {
            let mut st = self.st.lock().unwrap();
            let word = last_word(&st.draft).len();
            let keep = st.draft.len() - word;
            let mut new = st.draft[..keep].to_owned();
            new.push_str(&insert);
            new.push(' ');
            if let Ok(u) = OwnedUserId::try_from(user) {
                st.mentions.push((insert.clone(), u));
            }
            st.draft = new.clone();
            new
        };
        self.ui(move |u| {
            u.set_suggestions(ModelRc::default());
            u.set_draft(s(&new));
            u.set_cursor_at(new.len() as i32);
            u.set_cursor_request(u.get_cursor_request() + 1);
        });
    }

    /// Joined members of a room, fetched at most once a minute.
    async fn members(&self, key: &RoomKey, room: &matrix_sdk::Room) -> Vec<(OwnedUserId, String)> {
        if let Some((t, m)) = self.st.lock().unwrap().members_cache.get(key)
            && t.elapsed() < Duration::from_secs(60)
        {
            return m.clone();
        }
        let me = room.own_user_id().to_owned();
        let list: Vec<(OwnedUserId, String)> = room
            .members_no_sync(matrix_sdk::RoomMemberships::JOIN)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|m| m.user_id() != me)
            .map(|m| (m.user_id().to_owned(), m.display_name().unwrap_or(m.user_id().localpart()).to_owned()))
            .collect();
        self.st.lock().unwrap().members_cache.insert(key.clone(), (Instant::now(), list.clone()));
        list
    }

    /// The draft changed: tell the room we're typing, if that's turned on.
    async fn composing(&self, empty: bool) {
        if !self.settings.lock().unwrap().send_typing {
            return;
        }
        let Some((key, room)) = self.current_room().await else { return };
        // The server shows it for a few seconds; refresh at most every 3.
        let due = {
            let mut st = self.st.lock().unwrap();
            let fresh = st.typing_sent.as_ref().is_some_and(|(k, t)| *k == key && t.elapsed() < Duration::from_secs(3));
            if empty {
                st.typing_sent.take().is_some()
            } else if fresh {
                false
            } else {
                st.typing_sent = Some((key.clone(), Instant::now()));
                true
            }
        };
        if due && let Err(e) = room.typing_notice(!empty).await {
            tracing::debug!("typing notice failed: {e}");
        }
    }

    fn set_show_events(&self, on: bool) {
        let mut st = self.settings.lock().unwrap();
        st.show_events = on;
        let _ = st.save(self.mgr.data_dir());
        drop(st);
        if let Some(u) = self.ui.upgrade() {
            u.global::<Prefs>().set_show_events(on);
        }
        let c = self.clone();
        self.spawn(async move { c.show_messages().await });
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
            self.spawn(async move {
                c.show_messages().await;
                c.show_thread().await;
            });
        }
    }

    /// Works out a room's read receipts again soon (at most every half
    /// second), then redraws it.
    /// Looks up whether `account` verified each of `users` it hasn't
    /// checked yet (from the local crypto store, no network), and redraws
    /// when one turns out verified or changed.
    fn fetch_trust(&self, account: AccountId, users: impl Iterator<Item = OwnedUserId>) {
        let todo: Vec<OwnedUserId> = {
            let mut st = self.st.lock().unwrap();
            let mut todo: Vec<OwnedUserId> = users.filter(|u| !st.trust.contains_key(&(account, u.clone()))).collect();
            todo.sort();
            todo.dedup();
            for u in &todo {
                st.trust.insert((account, u.clone()), ui::Trust::Unknown);
            }
            todo
        };
        if todo.is_empty() {
            return;
        }
        let c = self.clone();
        self.spawn(async move {
            let Some(client) = c.mgr.client(account).await else { return };
            let mut changed = false;
            for user in todo {
                let t = match client.encryption().get_user_identity(&user).await {
                    Ok(Some(i)) if i.has_verification_violation() => ui::Trust::Changed,
                    Ok(Some(i)) if i.is_verified() => ui::Trust::Verified,
                    Ok(Some(_)) => ui::Trust::Unverified,
                    _ => ui::Trust::Unknown,
                };
                changed |= matches!(t, ui::Trust::Verified | ui::Trust::Changed);
                c.st.lock().unwrap().trust.insert((account, user), t);
            }
            if changed && c.current().is_some_and(|k| k.0 == account) {
                c.show_messages().await;
            }
        });
    }

    fn schedule_marks(&self, key: RoomKey) {
        if !self.st.lock().unwrap().marks_pending.insert(key.clone()) {
            return;
        }
        let c = self.clone();
        self.spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            c.st.lock().unwrap().marks_pending.remove(&key);
            c.refresh_marks(&key).await;
            if c.current().as_ref() == Some(&key) {
                c.show_messages().await;
            }
        });
    }

    async fn refresh_marks(&self, key: &RoomKey) {
        let Some(room) = self.mgr.client(key.0).await.and_then(|c| c.get_room(&key.1)) else { return };
        let marks = timeline::read_marks(&room).await;
        self.st.lock().unwrap().marks.insert(key.clone(), marks);
    }

    /// Redraws the thread panel, if a thread is open.
    async fn show_thread(&self) {
        let (Some(key), Some(root)) = (self.current(), self.st.lock().unwrap().thread.clone()) else { return };
        let rows = self.rows_for(&key, Some(&root)).await;
        self.fetch_avatars(key.0, rows.iter().filter_map(|r| r.item.sender_avatar.clone()));
        let items: Vec<HistoryItem> = rows.iter().map(|r| r.item.clone()).collect();
        self.fetch_pictures(&key, &items);
        let px = self.avatars.clone();
        self.ui(move |_| {
            ui::with(|m| {
                let items: Vec<MessageItem> = rows.iter().map(|r| m.message(r, &px)).collect();
                ui::sync_rows(&m.thread, items, |a, b| a.id == b.id);
            });
        });
    }

    async fn open_thread(&self, root: OwnedEventId) {
        self.st.lock().unwrap().thread = Some(root);
        self.ui(|u| {
            ui::with(|m| m.thread.set_vec(Vec::new()));
            u.set_profile_open(false);
            u.set_room_info_open(false);
            u.set_thread_open(true);
        });
        self.show_thread().await;
    }

    /// Sends `text` into the open thread.
    async fn thread_send(&self, text: String) {
        let text = text.trim().to_owned();
        let (Some((key, room)), Some(root)) = (self.current_room().await, self.st.lock().unwrap().thread.clone()) else { return };
        if text.is_empty() {
            return;
        }
        let latest = self.rows_for(&key, Some(&root)).await.last().map(|r| r.item.event_id.clone()).unwrap_or_else(|| root.clone());
        let me = room.own_user_id().to_owned();
        let account = self.mgr.accounts().await.into_iter().find(|a| a.id == key.0);
        let Ok(local) = OwnedEventId::try_from(format!("{}{}", timeline::LOCAL_ECHO, &whisker_core::vault::random_secret()[..12])) else { return };
        let item = HistoryItem {
            event_id: local.clone(),
            sender: me,
            sender_name: account.as_ref().and_then(|a| a.profile_name.clone()),
            sender_avatar: account.and_then(|a| a.avatar),
            body: text.clone(),
            ts: MilliSecondsSinceUnixEpoch::now(),
            reply_to: None,
            media: None,
            edited: false,
            notice: false,
            member: None,
            thread: Some(root.clone()),
            pills: Vec::new(),

            shield: None,
        };
        self.merge(&key, vec![item]);
        self.show_thread().await;
        self.update_message(&key, &root);
        match timeline::send_in_thread(&room, &root, &latest, &text).await {
            Ok(real) => self.settle_echo(&key, &local, Some(real)).await,
            Err(e) => {
                self.settle_echo(&key, &local, None).await;
                self.status(format!("Not sent: {e:#}"));
            }
        }
        self.show_thread().await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn login(&self, hs: String, user: String, pass: String, tags: String, proxy: String, agent: bool, register: bool, token: String) {
        let Some(vault) = self.st.lock().unwrap().active_vault.clone() else { return };
        let (hs, user) = split_login(&hs, &user);
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
        // This login (session): 2 verified, 1 not, 0 not signed in; and its device id.
        let mut session = (0, String::new());
        let mut identity = (false, false);
        let mut agent = AgentForm::default();
        let mut models: Vec<SharedString> = Vec::new();
        let mut image_models: Vec<SharedString> = Vec::new();
        let mut defaults = (String::new(), String::new());
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
            if let Some(client) = self.mgr.client(aid).await {
                let verified = match client.encryption().get_own_device().await {
                    Ok(Some(d)) if d.is_cross_signed_by_owner() => 2,
                    Ok(Some(_)) => 1,
                    _ => 0,
                };
                session = (verified, client.device_id().map(|d| d.to_string()).unwrap_or_default());
                identity = (whisker_core::verify::has_identity(&client).await, whisker_core::verify::recovery_on(&client));
            }
        }
        if let Some(rest) = id.strip_prefix("ai:")
            && let Ok(aid) = rest.parse::<AccountId>()
            && let Some(a) = accounts.iter().find(|a| a.id == aid)
        {
            let cfgs = agents_in(&self.mgr, &a.vault).await;
            let cfg = cfgs.iter().find(|c| c.account == aid);
            *VISION.lock().unwrap() = None;
            agent = agent_form::to_form(aid, &a.display_name, cfg);
            // Opening the page again drops an import that wasn't saved.
            self.st.lock().unwrap().pending_cards.remove(&aid);
            let png = self.card_picture(&a.vault, aid).await;
            self.show_card_picture(png);
            if let Some(d) = self.st.lock().unwrap().detected.get(&aid) {
                models = d.models.iter().map(s).collect();
                image_models = d.vision.iter().map(s).collect();
                defaults = (d.default_model.clone().unwrap_or_default(), d.default_vision.clone().unwrap_or_default());
                *VISION.lock().unwrap() = Some(d.vision.iter().cloned().collect());
                detect = format!("✓ {} · {} models", d.service, d.models.len());
            } else if let Some(c) = cfg
                && !c.api.url.is_empty()
            {
                // List the models in the background, so both pickers have them.
                detect = format!("✓ {}", c.api.service);
                let (me, url) = (self.clone(), c.api.url.clone());
                self.spawn(async move { me.detect_api(aid, url, String::new()).await });
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
                        session_verified: session.0,
                        session_id: s(&session.1),
                        has_identity: identity.0,
                        recovery_on: identity.1,
                        recovery_key: s(""),
                    }
                }
                None => AccountForm::default(),
            };
            u.set_account_form(account_form);
            u.set_agent_form(agent);
            u.set_models(ModelRc::new(VecModel::from(models)));
            u.set_image_models(ModelRc::new(VecModel::from(image_models)));
            u.set_default_model(s(&defaults.0));
            u.set_default_image_model(s(&defaults.1));
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
                let vision: Vec<SharedString> = d.vision.iter().map(s).collect();
                *VISION.lock().unwrap() = Some(d.vision.iter().cloned().collect());
                let chat = d.default_model.clone().unwrap_or_default();
                let eyes = d.default_vision.clone().unwrap_or_default();
                self.st.lock().unwrap().detected.insert(account, d);
                self.ui(move |u| {
                    u.set_models(ModelRc::new(VecModel::from(models)));
                    u.set_image_models(ModelRc::new(VecModel::from(vision)));
                    u.set_default_model(s(chat));
                    u.set_default_image_model(s(eyes));
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
        let (detected, pending) = {
            let st = self.st.lock().unwrap();
            (st.detected.get(&id).cloned(), st.pending_cards.get(&id).cloned())
        };
        let base = match &pending {
            Some(p) => p.as_ref().map(|(c, _)| c.clone()),
            None => old.as_ref().and_then(|o| o.card.clone()),
        };
        let cfg = agent_form::from_form(id, f, old.as_ref(), detected.as_ref(), base.as_ref())?;
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
        // A newly imported (or removed) card's picture.
        let pending = self.st.lock().unwrap().pending_cards.remove(&id);
        if let Some(p) = pending {
            let text = p.and_then(|(_, png)| png).map(|b| whisker_agent::card::encode_base64(&b)).unwrap_or_default();
            if let Err(e) = self.mgr.set_extra(&vault, &whisker_agent::card::picture_key(id), &text).await {
                tracing::warn!("couldn't save the card picture: {e:#}");
            }
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
        // Pictures on, but nothing that can look at them.
        let blind = cfg.see_images && cfg.image_model.trim().is_empty() && {
            let st = self.st.lock().unwrap();
            match st.detected.get(&id) {
                Some(d) if !d.vision.is_empty() => !d.vision.contains(&cfg.api.model),
                _ => !whisker_agent::api::looks_vision(&cfg.api.model),
            }
        };
        if blind {
            self.settings_msg(format!(
                "{msg} But {} may not be able to see pictures: pick a Picture model under \"Let the AI see pictures\", or it will guess at them.",
                cfg.api.model
            ));
        } else {
            self.settings_msg(msg);
        }
        let name = f.name.to_string();
        let form = agent_form::to_form(id, &name, Some(&cfg));
        self.ui(move |u| u.set_agent_form(form));
    }

    /// The saved picture of `account`'s character card.
    async fn card_picture(&self, vault: &str, account: AccountId) -> Option<Vec<u8>> {
        let text: String = self.mgr.extra(vault, &whisker_agent::card::picture_key(account)).await?;
        whisker_agent::card::decode_base64(&text).ok().filter(|b| !b.is_empty())
    }

    fn show_card_picture(&self, png: Option<Vec<u8>>) {
        let px = png.and_then(|b| decode(&b, 512, 512, false));
        self.ui(move |u| u.set_card_picture(px.map(Image::from_rgba8).unwrap_or_default()));
    }

    /// Character card buttons on an AI's page: "import", "export-json",
    /// "export-png", "remove" and "use-profile" (the card's name and
    /// picture for the AI's account).
    async fn card_action(&self, action: String, f: AgentForm) {
        let Ok(id) = f.account.parse::<AccountId>() else { return };
        let Some(vault) = self.vault_of(id).await else { return self.settings_msg("That account is locked.") };
        let saved = agents_in(&self.mgr, &vault).await.into_iter().find(|c| c.account == id);
        let pending = self.st.lock().unwrap().pending_cards.get(&id).cloned();
        // The card and picture as they stand: imported, else saved.
        let (base, png) = match pending {
            Some(p) => p.map(|(c, png)| (Some(c), png)).unwrap_or_default(),
            None => (saved.as_ref().and_then(|c| c.card.clone()), self.card_picture(&vault, id).await),
        };
        match action.as_str() {
            "import" => {
                let Some(bytes) = pick_file("Import a character card", &[("Character cards", &["png", "json"])]).await else { return };
                match whisker_agent::card::import(&bytes) {
                    Ok(imp) => {
                        let msg = format!(
                            "Imported {}{}. Check it below, then press Save.",
                            imp.card.name,
                            if imp.png.is_some() { " with its picture" } else { "" }
                        );
                        let form = f.clone().with_card(Some(&imp.card));
                        self.show_card_picture(imp.png.clone());
                        self.st.lock().unwrap().pending_cards.insert(id, Some((imp.card, imp.png)));
                        self.ui(move |u| u.set_agent_form(form));
                        self.settings_msg(msg);
                    }
                    Err(e) => self.settings_msg(format!("Couldn't import that card: {e:#}")),
                }
            }
            "remove" => {
                self.st.lock().unwrap().pending_cards.insert(id, None);
                let form = f.clone().with_card(None);
                self.show_card_picture(None);
                self.ui(move |u| u.set_agent_form(form));
                self.settings_msg("Card removed. Press Save to keep it that way.");
            }
            "export-json" | "export-png" => {
                let card = match agent_form::card_from_form(&f, base.as_ref()) {
                    Ok(Some(c)) => c,
                    Ok(None) => return self.settings_msg("There's no card to export."),
                    Err(e) => return self.settings_msg(format!("{e:#}")),
                };
                let png_out = action == "export-png";
                let bytes = if png_out {
                    // Cards travel inside a picture: the card's own, or one picked now.
                    let picture = match png {
                        Some(p) => p,
                        None => {
                            let Some(raw) = pick_file("Choose a picture for the card", &[("Images", &["png", "jpg", "jpeg", "webp", "gif"])]).await else { return };
                            match to_png(&raw) {
                                Some(p) => p,
                                None => return self.settings_msg("Couldn't read that picture."),
                            }
                        }
                    };
                    match card.to_png(&picture) {
                        Ok(b) => b,
                        Err(e) => return self.settings_msg(format!("Couldn't export: {e:#}")),
                    }
                } else {
                    serde_json::to_vec_pretty(&card.to_v3()).expect("cards serialize")
                };
                let name = format!("{}.{}", safe_file_name(&card.name), if png_out { "png" } else { "json" });
                let Some(path) = save_file_as("Export character card", name).await else { return };
                self.settings_msg(match tokio::fs::write(&path, bytes).await {
                    Ok(()) => format!("Exported to {}", path.display()),
                    Err(e) => format!("Couldn't export: {e}"),
                });
            }
            "use-profile" => {
                let Some(client) = self.mgr.client(id).await else { return };
                let name = f.card_name.trim().to_owned();
                if name.is_empty() {
                    return self.settings_msg("The character needs a name first.");
                }
                self.settings_msg("Updating the AI's profile…");
                let mut r = whisker_core::rooms::set_display_name(&client, &name).await;
                if r.is_ok()
                    && let Some(p) = png
                {
                    r = whisker_core::rooms::set_avatar_data(&client, p).await;
                }
                match r {
                    Ok(()) => {
                        self.mgr.refresh_profile(id).await;
                        self.refresh_accounts().await;
                        self.settings_msg(format!("The AI's account is now called {name}."));
                    }
                    Err(e) => self.settings_msg(format!("Couldn't update the profile: {e:#}")),
                }
            }
            _ => {}
        }
    }

    /// Narrows the AI page's model list to names containing `q`.
    async fn filter_models(&self, q: String) {
        let Some(id) = self.settings_account() else { return };
        let q = q.trim().to_lowercase();
        // Nothing listed yet: leave whatever the page shows.
        let Some(all) = self.st.lock().unwrap().detected.get(&id).map(|d| d.models.clone()) else { return };
        // The exact name picked: show the whole list again.
        let models: Vec<SharedString> = if q.is_empty() || all.iter().any(|m| m.to_lowercase() == q) {
            all.iter().map(s).collect()
        } else {
            all.iter().filter(|m| m.to_lowercase().contains(&q)).map(s).collect()
        };
        self.ui(move |u| u.set_models(ModelRc::new(VecModel::from(models))));
    }

    async fn filter_image_models(&self, q: String) {
        let Some(id) = self.settings_account() else { return };
        let q = q.trim().to_lowercase();
        let Some(all) = self.st.lock().unwrap().detected.get(&id).map(|d| d.vision.clone()) else { return };
        let models: Vec<SharedString> = if q.is_empty() || all.iter().any(|m| m.to_lowercase() == q) {
            all.iter().map(s).collect()
        } else {
            all.iter().filter(|m| m.to_lowercase().contains(&q)).map(s).collect()
        };
        self.ui(move |u| u.set_image_models(ModelRc::new(VecModel::from(models))));
    }

    async fn test_agent(&self, f: AgentForm) {
        self.ui(|u| u.set_test_status(s("Asking the model…")));
        let msg = match self.agent_config(&f).await {
            Err(e) => format!("✗ {e:#}"),
            Ok((_, _, cfg)) => match Llm::new(cfg.api.clone(), cfg.max_tokens.min(300), cfg.extra_params.clone()).map(|l| l.with_venice(cfg.venice())) {
                Err(e) => format!("✗ {e:#}"),
                Ok(llm) => match tokio::time::timeout(Duration::from_secs(60), llm.test()).await {
                    Ok(Ok(reply)) => format!("✓ It works. The model said: “{}”", short_reply(&reply)),
                    Ok(Err(e)) => format!("✗ {e:#}"),
                    Err(_) => "✗ No answer after a minute. The model may be overloaded, or the address may be wrong.".into(),
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
                ClientEvent::Message { account, room_id, event_id, sender, sender_name, sender_avatar, body, origin_server_ts, reply_to, media, thread, pills, shield, .. } => {
                    {
                        let c = self.clone();
                        let (room_id, sender, name, body) = (room_id.clone(), sender.clone(), sender_name.clone(), body.clone());
                        self.spawn(async move { c.maybe_notify(account, room_id, sender, name, body, origin_server_ts).await });
                    }
                    let key = (account, room_id);
                    // New since the last sync, from someone else, in a room that
                    // isn't open: counts towards the room's badge.
                    let live = self.st.lock().unwrap().synced.contains(&account);
                    if live && self.current().as_ref() != Some(&key) && !self.mgr.accounts().await.iter().any(|a| a.user_id == sender.as_str()) {
                        self.st.lock().unwrap().fresh.entry(key.clone()).or_default().insert(event_id.clone());
                    }
                    let item = HistoryItem { event_id, sender, sender_name, sender_avatar, body, ts: origin_server_ts, reply_to, media: media.map(|m| *m), edited: false, notice: false, member: None, thread: thread.clone(), pills, shield };
                    // Kept for every room, so opening one shows something at once
                    // (its history still loads on first open).
                    self.merge(&key, vec![item]);
                    {
                        if self.current().as_ref() == Some(&key) {
                            self.st.lock().unwrap().fresh.remove(&key);
                            // A thread reply updates its thread, not the timeline's end.
                            match &thread {
                                Some(root) => self.update_message(&key, root),
                                None => self.append_last(&key).await,
                            }
                            let c = self.clone();
                            let k = key.clone();
                            self.spawn(async move { c.mark_read(&k).await });
                        }
                    }
                    if selected(account) {
                        self.schedule_rooms();
                    }
                }
                ClientEvent::Notice { account, room_id, item } => {
                    let key = (account, room_id);
                    if self.st.lock().unwrap().older.contains_key(&key) {
                        self.merge(&key, vec![*item]);
                        if self.current().as_ref() == Some(&key) && self.settings.lock().unwrap().show_events {
                            self.append_last(&key).await;
                        }
                    }
                }
                ClientEvent::Typing { account, room_id, users } => {
                    let key = (account, room_id);
                    self.st.lock().unwrap().typing.insert(key.clone(), users);
                    if self.current().as_ref() == Some(&key) {
                        self.show_typing(&key).await;
                    }
                }
                ClientEvent::RoomKeys { account, rooms } => {
                    for room_id in rooms {
                        self.redecrypt((account, room_id));
                    }
                }
                ClientEvent::Receipts { account, room_id } => {
                    let key = (account, room_id);
                    if self.current().as_ref() == Some(&key) {
                        self.schedule_marks(key);
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
                ClientEvent::Reaction { account, room_id, reaction } => {
                    let key = (account, room_id);
                    if self.st.lock().unwrap().older.contains_key(&key) {
                        let target = reaction.target.clone();
                        self.merge_reactions(&key, vec![reaction]);
                        self.update_message(&key, &target);
                    }
                }
                ClientEvent::Redacted { account, room_id, target } => {
                    let key = (account, room_id);
                    // A taken-back reaction, or a deleted message.
                    let unreacted = {
                        let mut st = self.st.lock().unwrap();
                        let list = st.reactions.entry(key.clone()).or_default();
                        let hit = list.iter().find(|r| r.event_id == target).map(|r| r.target.clone());
                        list.retain(|r| r.event_id != target);
                        hit
                    };
                    match unreacted {
                        Some(message) => self.update_message(&key, &message),
                        None => self.remove_message(&key, &target),
                    }
                }
                ClientEvent::Verification { account, user, flow_id, state } => {
                    let c = self.clone();
                    self.spawn(async move { c.on_verification(account, user, flow_id, state).await });
                }
                ClientEvent::AccountError { message, .. } => self.status(tidy_error(&message)),
            }
        }
    }
}

/// Leaves out "X joined" lines for people already in the room. Some servers
/// don't say what a member event replaced, so every name or picture change
/// would otherwise read as another join.
fn drop_repeat_joins(msgs: &mut Vec<HistoryItem>) {
    let mut joined: HashMap<OwnedUserId, bool> = HashMap::new();
    msgs.retain(|m| match &m.member {
        Some(c) => {
            let repeat = c.guessed && joined.get(&c.user) == Some(&true);
            joined.insert(c.user.clone(), c.joined);
            !repeat
        }
        None => {
            if !m.notice {
                joined.entry(m.sender.clone()).or_insert(true);
            }
            true
        }
    });
}

/// A model's test reply, fit for one status line: without "thinking"
/// sections, on one line, at most 300 characters.
fn short_reply(reply: &str) -> String {
    let mut text = reply.to_owned();
    while let (Some(a), Some(b)) = (text.find("<think>"), text.find("</think>")) {
        if b < a {
            break;
        }
        text.replace_range(a..b + "</think>".len(), "");
    }
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match one_line.char_indices().nth(300) {
        Some((i, _)) => format!("{}…", &one_line[..i]),
        None => one_line,
    }
}

/// Members matching a half-typed `@name`, closest first: display name
/// starting with it, then a word of it, then the user id, then anywhere.
fn best_members(members: &[(OwnedUserId, String)], q: &str) -> Vec<(OwnedUserId, String)> {
    let q = q.to_lowercase();
    let mut scored: Vec<(u8, usize, &(OwnedUserId, String))> = members
        .iter()
        .filter_map(|m| {
            let name = m.1.to_lowercase();
            let local = m.0.localpart().to_lowercase();
            let score = if name.starts_with(&q) {
                0
            } else if name.split_whitespace().any(|w| w.starts_with(&q)) {
                1
            } else if local.starts_with(&q) {
                2
            } else if name.contains(&q) || local.contains(&q) {
                3
            } else {
                return None;
            };
            Some((score, name.chars().count(), m))
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then_with(|| a.2.1.cmp(&b.2.1)));
    scored.into_iter().map(|(_, _, m)| m.clone()).collect()
}

/// The word at the end of `text` (what's being typed).
fn last_word(text: &str) -> &str {
    let start = text.rfind(char::is_whitespace).map_or(0, |i| i + text[i..].chars().next().map_or(1, char::len_utf8));
    &text[start..]
}

/// Our own Matrix name (localpart), for spotting mentions.
fn sender_of_me(client: &matrix_sdk::Client) -> String {
    client.user_id().map(|u| u.localpart().to_owned()).unwrap_or_default()
}

/// Reads a file the user picks (desktop only).
async fn pick_file(title: &'static str, filters: &'static [(&'static str, &'static [&'static str])]) -> Option<Vec<u8>> {
    #[cfg(not(target_os = "android"))]
    {
        let path = tokio::task::spawn_blocking(move || {
            let mut d = rfd::FileDialog::new().set_title(title);
            for (name, ext) in filters {
                d = d.add_filter(*name, ext);
            }
            d.pick_file()
        })
        .await
        .ok()
        .flatten()?;
        tokio::fs::read(path).await.ok()
    }
    #[cfg(target_os = "android")]
    {
        let _ = (title, filters);
        None
    }
}

/// Asks which file to open (desktop only).
async fn pick_path(title: &'static str) -> Option<PathBuf> {
    #[cfg(not(target_os = "android"))]
    {
        tokio::task::spawn_blocking(move || rfd::FileDialog::new().set_title(title).pick_file()).await.ok().flatten()
    }
    #[cfg(target_os = "android")]
    {
        let _ = title;
        None
    }
}

/// Asks where to save a file (desktop only).
async fn save_file_as(title: &'static str, name: String) -> Option<PathBuf> {
    #[cfg(not(target_os = "android"))]
    {
        tokio::task::spawn_blocking(move || rfd::FileDialog::new().set_title(title).set_file_name(name).save_file()).await.ok().flatten()
    }
    #[cfg(target_os = "android")]
    {
        let _ = (title, name);
        None
    }
}

/// Any picture, as PNG bytes.
fn to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory(bytes).ok()?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// Puts the upload settings on the Settings page.
fn show_upload_prefs(p: &Prefs, u: &whisker_core::upload::UploadOptions) {
    use whisker_core::upload::{Convert, ThumbFormat};
    p.set_up_strip(u.strip_metadata);
    p.set_up_hash(u.hash_names);
    p.set_up_convert(match u.convert {
        Convert::Keep => 0,
        Convert::Jpeg => 1,
        Convert::Png => 2,
    });
    p.set_up_quality(i32::from(u.quality));
    p.set_up_thumbs(u.thumbnails);
    p.set_up_thumb_format(i32::from(u.thumb_format == ThumbFormat::Png));
    p.set_up_thumb_quality(i32::from(u.thumb_quality));
    p.set_up_encrypt(u.always_encrypt);
}

/// Files uploading at the same time.
const UPLOADS_AT_ONCE: usize = 3;

/// Picked files on their way, for the status line.
#[derive(Default)]
struct Uploads {
    total: usize,
    done: usize,
    /// Uploading now: (name, percent sent), by place in the list.
    active: std::collections::BTreeMap<usize, (String, u8)>,
}

impl Uploads {
    fn line(&self) -> String {
        let now: Vec<String> = self
            .active
            .values()
            .map(|(n, p)| format!("{n} {p}%"))
            .collect();
        match (self.total, now.is_empty()) {
            (1, false) => format!("Uploading {}…", now[0]),
            (_, false) => format!(
                "Uploading {} of {} files… {}",
                (self.done + 1).min(self.total),
                self.total,
                now.join(", ")
            ),
            _ => format!(
                "Sending {} of {} files…",
                (self.done + 1).min(self.total),
                self.total
            ),
        }
    }
}

/// A name usable as a file name everywhere.
fn safe_file_name(name: &str) -> String {
    let n: String = name.chars().map(|c| if c.is_alphanumeric() || " -_".contains(c) { c } else { '_' }).collect();
    let n = n.trim();
    if n.is_empty() { "character".into() } else { n.chars().take(60).collect() }
}

/// Emoji in rows of eight, for the picker grid.
fn emoji_rows(list: &[&str]) -> ModelRc<EmojiRow> {
    let rows: Vec<EmojiRow> = list
        .chunks(8)
        .map(|c| EmojiRow { items: ModelRc::new(VecModel::from(c.iter().map(|e| EmojiCell { symbol: s(e), picture: ui::emoji_image(e) }).collect::<Vec<_>>())) })
        .collect();
    ModelRc::new(VecModel::from(rows))
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

/// Servers offered first when signing in or creating an account.
const SUGGESTED_SERVERS: &[&str] = &["anoncat.gleeze.com"];

/// A full address typed as the username (`@name:server`) picks its own
/// server; otherwise the username's `@` is dropped and `hs` is used.
fn split_login(hs: &str, user: &str) -> (String, String) {
    let user = user.trim();
    match user.strip_prefix('@').unwrap_or(user).split_once(':') {
        Some((name, server)) if !server.is_empty() => (server.to_owned(), name.to_owned()),
        _ => (hs.trim().to_owned(), user.trim_start_matches('@').to_owned()),
    }
}

pub fn run(data_dir: PathBuf) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let ui = AppWindow::new()?;
    #[cfg(not(target_os = "android"))]
    select::install(&ui);
    ui::install(&ui);
    let mgr = AccountManager::new(data_dir);
    ui.set_first_run(mgr.is_empty());

    let prefs = AppSettings::load(mgr.data_dir());
    ui.set_theme(prefs.theme.index());
    ui.set_auto_lock(prefs.auto_lock_index());
    ui.set_encryption_summary(ENCRYPTION_SUMMARY.into());
    ui.global::<Prefs>().set_rich_text(prefs.rich_text);
    ui.global::<Prefs>().set_show_events(prefs.show_events);
    ui.global::<Prefs>().set_notify_level(prefs.notify_level);
    ui.global::<Prefs>().set_send_typing(prefs.send_typing);
    ui.global::<Prefs>().set_autocorrect(prefs.autocorrect);
    ui.global::<Prefs>().set_spell_check(prefs.spell_check);
    #[cfg(not(target_os = "android"))]
    spell_marks::set_on(&ui, prefs.spell_check);
    ui.global::<Prefs>().set_link_posters(prefs.link_posters);
    ui.global::<Prefs>()
        .set_ask_file_servers(prefs.ask_file_servers);
    show_upload_prefs(&ui.global::<Prefs>(), &prefs.upload);
    whisker_core::media_fetch::set_ask_file_servers(prefs.ask_file_servers);
    ui.global::<Prefs>().set_ask_room_servers(prefs.ask_room_servers);
    whisker_core::media_fetch::set_ask_room_servers(prefs.ask_room_servers);
    ui.global::<Prefs>()
        .set_clear_command(s(&prefs.clear_command));
    autocorrect::warm();
    ui.global::<ModelInfo>().on_sees(|model, _| {
        let model = model.trim();
        match VISION.lock().unwrap().as_ref() {
            Some(_) if model.is_empty() => 0,
            Some(seeing) if seeing.contains(model) => 1,
            Some(_) => 2,
            None => 0,
        }
    });
    ui.global::<TextTools>().on_bytes(|text| text.len() as i32);
    ui.global::<TextTools>().on_slice(|text, a, b| {
        let (a, b) = (a.clamp(0, text.len() as i32) as usize, b.clamp(0, text.len() as i32) as usize);
        let (from, to) = (a.min(b), a.max(b));
        text.get(from..to).map(s).unwrap_or_default()
    });
    apply_menu(&ui, &prefs.menu_hidden);
    let servers: Vec<SharedString> = SUGGESTED_SERVERS.iter().map(s).chain([s("Other server…")]).collect();
    ui.set_login_servers(slint::ModelRc::new(slint::VecModel::from(servers)));
    ui::RICH.set(prefs.rich_text);

    let ctl = Ctl {
        rt: rt.handle().clone(),
        mgr,
        ui: ui.as_weak(),
        st: Arc::default(),
        settings: Arc::new(Mutex::new(prefs)),
        avatars: Arc::default(),
        avatar_tries: Arc::default(),
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
    on!(on_share_pick_account, |id| share_pick_account);
    on!(on_space_join, |id| space_join);
    on!(on_space_preview, |id| space_preview);
    on!(on_explore_preview, |id| explore_preview);
    on!(on_preview_join, preview_join);
    on!(on_preview_close, preview_close);
    on!(on_share_search, |q| share_search);
    on!(on_share_to, |id| share_to);
    {
        let c = ctl.clone();
        ui.on_share_back(move || c.share_back());
        let c = ctl.clone();
        ui.on_share_cancel(move || c.share_cancel());
    }
    // A newer build opened: close (locking everything) so it takes over.
    share::listen(ctl.mgr.data_dir(), || {
        let _ = slint::invoke_from_event_loop(|| {
            let _ = slint::quit_event_loop();
        });
    });
    {
        let c = ctl.clone();
        share::on_share(move |items| c.receive_share(items));
    }
    on!(on_unlock_more, |p| unlock_more);
    on!(on_create_hidden, |p| create_hidden);
    on!(on_select_account, |id| select_account);
    on!(on_select_room, |id| select_room);
    on!(on_search_rooms, |q| search_rooms);
    on!(on_send, |t| send);
    on!(on_message_action, |a, id| message_action);
    on!(on_clear_messages, |n| clear_messages);
    #[cfg(target_os = "linux")]
    {
        let c = ctl.clone();
        ui.global::<Player>().on_toggle(move |id| {
            if player::is_loaded(&id) {
                player::toggle(&c.ui);
            } else {
                let (c2, id) = (c.clone(), id.to_string());
                c.spawn(async move { c2.message_action("play".into(), id).await });
            }
        });
        let c = ctl.clone();
        ui.global::<Player>().on_seek(move |f| player::seek(&c.ui, f));
        let c = ctl.clone();
        ui.global::<Player>().on_mute(move || player::mute(&c.ui));
        let c = ctl.clone();
        ui.global::<Player>().on_toggle_subtitles(move || player::subtitles(&c.ui));
        let c = ctl.clone();
        ui.global::<Player>().on_set_large(move |on| player::set_large(&c.ui, on));
    }
    #[cfg(not(target_os = "linux"))]
    {
        let c = ctl.clone();
        ui.global::<Player>().on_toggle(move |id| {
            let (c2, id) = (c.clone(), id.to_string());
            c.spawn(async move { c2.message_action("play".into(), id).await });
        });
    }
    on!(on_profile_action, |a, user| profile_action);
    on!(on_profile_requested, |user| profile_requested);
    on!(on_verify_action, |a| verify_action);
    on!(on_profile_edit, |a, id, v| profile_edit);
    on!(on_open_room_info, open_room_info);
    on!(on_room_invite, |users| room_invite);
    on!(on_room_edit, |n, t| room_edit);
    on!(on_room_avatar, |a| room_avatar);
    on!(on_forward_search, |q| forward_search);
    on!(on_forward_to, |id| forward_to);
    {
        let c = ctl.clone();
        ui.on_emoji_search(move |q| c.emoji_search(q.to_string()));
    }
    ui.set_emoji_rows(emoji_rows(&emoji::search("")));
    let quick: Vec<QuickReaction> = emoji::QUICK
        .iter()
        .zip(emoji::QUICK_LABELS)
        .map(|(e, label)| QuickReaction {
            emoji: s(*e),
            label: s(label),
            icon: emoji::picture(e)
                .and_then(|png| decode(png, 32, 32, false))
                .map(Image::from_rgba8)
                .unwrap_or_default(),
        })
        .collect();
    ui.set_quick_reactions(ModelRc::new(VecModel::from(quick)));
    on!(on_create_room, |n, t, public, alias, enc, inv| create_room);
    on!(on_login, |hs, u, p, t, px, ag, reg, tok| login);
    {
        // A dragged account moves in the rail at once; the saved order follows.
        let c = ctl.clone();
        ui.on_account_menu(move |a, id| {
            if let Some(n) = a.strip_prefix("move:").and_then(|n| n.parse::<i64>().ok()) {
                ui::with(|m| {
                    let Some(i) = (0..m.accounts.row_count()).find(|&i| m.accounts.row_data(i).is_some_and(|r| r.id == id)) else { return };
                    let to = (i as i64 + n).clamp(0, m.accounts.row_count() as i64 - 1) as usize;
                    if to != i {
                        let row = m.accounts.remove(i);
                        m.accounts.insert(to, row);
                    }
                });
            }
            let c2 = c.clone();
            let (a, id) = (a.to_string(), id.to_string());
            c.spawn(async move { c2.account_menu(a, id).await });
        });
    }
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
    on!(on_card_action, |a, f| card_action);
    on!(on_filter_models, |q| filter_models);
    on!(on_filter_image_models, |q| filter_image_models);
    on!(on_change_passphrase, |o, n| change_passphrase);
    on!(on_attach, attach);
    on!(on_cancel_reply, cancel_reply);
    on!(on_save_sticker, |n| save_sticker);
    on!(on_remove_sticker, |n| remove_sticker);
    on!(on_send_sticker, |n| send_sticker);
    on!(on_open_stickers, open_stickers);
    on!(on_open_topic, open_topic);
    {
        let c = ctl.clone();
        ui.on_remove_pending_file(move |i| c.remove_pending_file(i));
    }
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
    {
        // Mentions link to matrix.to: open that person's profile instead.
        let c = ctl.clone();
        ui.on_open_link(move |l| match room_link(&l) {
            Some(RoomLink::User(user)) => {
                let c2 = c.clone();
                c.spawn(async move { c2.profile_requested(user).await });
            }
            // A room's link opens it, or a preview of it.
            Some(RoomLink::Room(target, via)) => {
                let c2 = c.clone();
                c.spawn(async move { c2.open_preview(target, via).await });
            }
            None => open_link(&l),
        });
    }
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
        ui.on_set_show_events(move |b| c.set_show_events(b));
    }
    {
        let c = ctl.clone();
        ui.on_set_notify_level(move |i| c.set_notify_level(i));
    }
    {
        let c = ctl.clone();
        ui.on_set_send_typing(move |b| c.set_send_typing(b));
        let c = ctl.clone();
        ui.on_set_autocorrect(move |b| c.set_autocorrect(b));
        let c = ctl.clone();
        ui.on_set_spell_check(move |b| c.set_spell_check(b));
        #[cfg(not(target_os = "android"))]
        {
            let weak = ui.as_weak();
            ui.on_spell_refresh(move || {
                if let Some(u) = weak.upgrade() {
                    spell_marks::soon(&u);
                }
            });
            let weak = ui.as_weak();
            ui.on_spell_replace(move |w| {
                if let Some(u) = weak.upgrade() {
                    spell_marks::replace(&u, &w);
                }
            });
            let (c, weak) = (ctl.clone(), ui.as_weak());
            ui.on_spell_add(move || {
                let (Some(u), Some(word)) = (weak.upgrade(), spell_marks::take_clicked_word())
                else {
                    return;
                };
                spell::add_custom([word.clone()]);
                spell_marks::recheck(&u);
                let c2 = c.clone();
                c.spawn(async move { c2.save_spell_word(word).await });
            });
        }
        let c = ctl.clone();
        ui.on_set_link_posters(move |b| c.set_link_posters(b));
        let c = ctl.clone();
        ui.on_set_ask_file_servers(move |b| c.set_ask_file_servers(b));
        let c = ctl.clone();
        ui.on_set_ask_room_servers(move |b| c.set_ask_room_servers(b));
        let c = ctl.clone();
        ui.on_set_upload(move |k, v| c.set_upload(k.as_str(), v));
        let c = ctl.clone();
        ui.on_set_clear_command(move |t| c.set_clear_command(t.to_string()));
        let c = ctl.clone();
        ui.on_set_menu_entry(move |k, b| c.set_menu_entry(k.to_string(), b));
    }
    {
        let c = ctl.clone();
        let weak = ui.as_weak();
        ui.on_draft_edited(move |t| {
            let mut text = t.to_string();
            if let (Some(new), Some(u)) = (c.fix_typing(&text), weak.upgrade()) {
                u.set_draft(s(&new));
                u.set_cursor_at(new.len() as i32);
                u.set_cursor_request(u.get_cursor_request() + 1);
                text = new;
            }
            let c2 = c.clone();
            c.spawn(async move { c2.draft_edited(text).await });
        });
    }
    on!(on_thread_send, |t| thread_send);
    {
        let c = ctl.clone();
        ui.on_pick_suggestion(move |insert, user| c.pick_suggestion(insert.to_string(), user.to_string()));
    }
    {
        let c = ctl.clone();
        ui.on_activity(move || c.touch());
    }

    rt.spawn(ctl.clone().pump());
    rt.spawn(ctl.clone().auto_lock_loop());
    #[cfg(all(unix, not(target_os = "android")))]
    rt.spawn(quit_on_signal());
    // Lock (saving the stores into the vaults) however the window loop ends,
    // even when it ends with an error (the display went away at logout).
    let ran = ui.run();
    rt.block_on(ctl.mgr.lock_all());
    let _ = std::fs::remove_dir_all(media_dir(ctl.mgr.data_dir()));
    share::stop_listening(ctl.mgr.data_dir());
    Ok(ran?)
}

/// Ends the window loop when the system asks Whisker to stop (shutdown,
/// logout, `kill`, Ctrl+C in a terminal), so it locks and saves like a
/// normal close. Without this the newest encryption keys, which live in a
/// RAM folder until saved into the vault, would be lost.
#[cfg(all(unix, not(target_os = "android")))]
async fn quit_on_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut hup), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
        signal(SignalKind::interrupt()),
    ) else {
        tracing::warn!("can't watch for shutdown signals");
        return;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = hup.recv() => {}
        _ = int.recv() => {}
    }
    tracing::info!("asked to stop: locking and saving");
    let _ = slint::invoke_from_event_loop(|| {
        let _ = slint::quit_event_loop();
    });
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    use std::sync::atomic::{AtomicI32, Ordering};
    // The task Whisker's window is in (0 until it's running).
    static TASK: AtomicI32 = AtomicI32::new(0);
    let shares = share::android::intent_shares(&app);
    // Sharing to Whisker while it's open starts a second copy of the
    // activity (in the sharing app's task). Hand the share to the running
    // one, bring it to the front, and close this one.
    let task = TASK.load(Ordering::SeqCst);
    if task != 0 {
        share::deliver(shares);
        share::android::bring_to_front(&app, task);
        return;
    }
    TASK.store(share::android::task_id(&app).unwrap_or(-1), Ordering::SeqCst);
    if !shares.is_empty() {
        share::deliver(shares);
    }
    let dir = app.internal_data_path().unwrap_or_else(|| PathBuf::from("/data/local/tmp/whisker"));
    slint::android::init(app).expect("Slint Android backend");
    if let Err(e) = run(dir) {
        eprintln!("whisker: {e:#}");
    }
    TASK.store(0, Ordering::SeqCst);
}






/// The models on the open AI page's service that can see pictures (None
/// until it's listed), for `ModelInfo.sees`.
static VISION: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Shows the message menu entries that aren't in `hidden`.
/// Message menu entries that are off until turned on in Settings.
const MENU_OFF_BY_DEFAULT: &[&str] = &["thread", "quote", "open-media", "copy-link", "profile", "copy-id", "source"];

/// Whether a message menu entry shows. `hidden` holds the entries turned
/// off, and "+entry" for ones that are off by default but turned on.
fn menu_entry_on(hidden: &std::collections::BTreeSet<String>, key: &str) -> bool {
    if MENU_OFF_BY_DEFAULT.contains(&key) { hidden.contains(&format!("+{key}")) } else { !hidden.contains(key) }
}

fn apply_menu(ui: &AppWindow, hidden: &std::collections::BTreeSet<String>) {
    let p = ui.global::<Prefs>();
    let on = |k: &str| menu_entry_on(hidden, k);
    p.set_menu_react(on("react"));
    p.set_menu_reply(on("reply"));
    p.set_menu_thread(on("thread"));
    p.set_menu_forward(on("forward"));
    p.set_menu_quote(on("quote"));
    p.set_menu_copy(on("copy"));
    p.set_menu_open_media(on("open-media"));
    p.set_menu_open_links(on("open-links"));
    p.set_menu_copy_link(on("copy-link"));
    p.set_menu_copy_file(on("copy-file"));
    p.set_menu_profile(on("profile"));
    p.set_menu_copy_id(on("copy-id"));
    p.set_menu_source(on("source"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_room_links() {
        assert_eq!(room_link("https://matrix.to/#/@amy:example.org"), Some(RoomLink::User("@amy:example.org".into())));
        assert_eq!(room_link("https://matrix.to/#/#rust:matrix.org"), Some(RoomLink::Room("#rust:matrix.org".into(), vec![])));
        assert_eq!(
            room_link("https://matrix.to/#/!abc:example.org?via=example.org&via=matrix.org"),
            Some(RoomLink::Room("!abc:example.org".into(), vec!["example.org".into(), "matrix.org".into()]))
        );
        assert_eq!(room_link("https://matrix.to/#/!abc:example.org/$ev?via=x.org"), Some(RoomLink::Room("!abc:example.org".into(), vec!["x.org".into()])));
        assert_eq!(room_link("https://example.org/page"), None);
    }

    #[test]
    fn previews_try_every_way() {
        let mxc = |s: &str| MediaSource::Plain(OwnedMxcUri::from(format!("mxc://other.org/{s}")));
        let media = |kind, thumbnail: Option<MediaSource>, mime: &str, size| timeline::Media {
            kind,
            source: mxc("file"),
            thumbnail,
            name: "x".into(),
            mime: Some(mime.into()),
            size: Some(size),
            width: None,
            height: None,
            linked: false,
        };
        let ways = |m: &timeline::Media| preview_attempts(m).into_iter().map(|(s, px, p)| (media_address(&s).rsplit('/').next().unwrap().to_owned(), px, p)).collect::<Vec<_>>();
        // The sender's thumbnail, the server's, then the picture itself.
        assert_eq!(
            ways(&media(MediaKind::Image, Some(mxc("thumb")), "image/jpeg", 1000)),
            [("thumb".into(), None, Preview::Still), ("file".into(), Some(640), Preview::Still), ("file".into(), None, Preview::Still)]
        );
        // Too big to fetch whole: thumbnails only.
        assert_eq!(ways(&media(MediaKind::Image, None, "image/avif", 90 << 20)), [("file".into(), Some(640), Preview::Still)]);
        assert_eq!(ways(&media(MediaKind::Image, None, "image/gif", 1000))[0], ("file".into(), None, Preview::Animated));
        // A video without a poster: read a frame through the relay, else download it.
        assert_eq!(ways(&media(MediaKind::Video, None, "video/mp4", 1000)), [("file".into(), None, Preview::Remote), ("file".into(), None, Preview::Video)]);
        assert!(ways(&media(MediaKind::File, None, "application/pdf", 10)).is_empty());
    }

    #[test]
    fn menu_entries_keep_their_defaults() {
        let mut hidden = std::collections::BTreeSet::new();
        assert!(menu_entry_on(&hidden, "reply"));
        assert!(!menu_entry_on(&hidden, "source"));
        hidden.insert("reply".to_owned());
        hidden.insert("+source".to_owned());
        assert!(!menu_entry_on(&hidden, "reply"));
        assert!(menu_entry_on(&hidden, "source"));
    }

    #[test]
    fn repeated_joins_are_dropped() {
        use whisker_core::timeline::MemberChange;
        let item = |id: &str, who: &str, member: Option<(bool, bool)>| HistoryItem {
            event_id: OwnedEventId::try_from(format!("${id}")).unwrap(),
            sender: OwnedUserId::try_from(format!("@{who}:x")).unwrap(),
            sender_name: None,
            sender_avatar: None,
            body: id.into(),
            ts: MilliSecondsSinceUnixEpoch::now(),
            reply_to: None,
            media: None,
            edited: false,
            notice: member.is_some(),
            member: member.map(|(joined, guessed)| MemberChange { user: OwnedUserId::try_from(format!("@{who}:x")).unwrap(), joined, guessed }),
            thread: None,
            pills: Vec::new(),
            shield: None,
        };
        let mut msgs = vec![
            item("join1", "nully", Some((true, true))),
            item("join2", "nully", Some((true, true))),
            item("hi", "bob", None),
            item("bobjoin", "bob", Some((true, true))),
            item("left", "nully", Some((false, false))),
            item("join3", "nully", Some((true, true))),
            item("join4", "nully", Some((true, false))),
        ];
        drop_repeat_joins(&mut msgs);
        let left: Vec<&str> = msgs.iter().map(|m| m.body.as_str()).collect();
        assert_eq!(left, ["join1", "hi", "left", "join3", "join4"]);
    }

    #[test]
    fn closest_member_first() {
        let m = |id: &str, name: &str| (OwnedUserId::try_from(id).unwrap(), name.to_owned());
        let list = [m("@zed:x", "Annabelle"), m("@bob:x", "Bob Ann"), m("@ann:x", "Ann"), m("@xan:x", "Roxanne"), m("@q:x", "Quinn")];
        let names: Vec<String> = best_members(&list, "ann").into_iter().map(|x| x.1).collect();
        assert_eq!(names, ["Ann", "Annabelle", "Bob Ann", "Roxanne"]);
        assert_eq!(best_members(&list, "zed")[0].1, "Annabelle");
    }

    #[test]
    fn test_replies_are_short() {
        assert_eq!(short_reply("<think>long\nthoughts</think>\n\nHi  there!"), "Hi there!");
        assert_eq!(short_reply(&"a".repeat(400)).chars().count(), 301);
    }

    #[test]
    fn last_words() {
        assert_eq!(last_word("hi @bo"), "@bo");
        assert_eq!(last_word("x :cat"), ":cat");
        assert_eq!(last_word("é😀"), "é😀");
        assert_eq!(last_word("a\n@z"), "@z");
    }

    #[test]
    fn login_names() {
        assert_eq!(split_login("anoncat.gleeze.com", "nully"), ("anoncat.gleeze.com".into(), "nully".into()));
        assert_eq!(split_login("anoncat.gleeze.com", " @nully "), ("anoncat.gleeze.com".into(), "nully".into()));
        assert_eq!(split_login("anoncat.gleeze.com", "@bob:matrix.org"), ("matrix.org".into(), "bob".into()));
        assert_eq!(split_login("", "bob:example.com"), ("example.com".into(), "bob".into()));
    }
}
