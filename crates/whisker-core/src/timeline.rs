//! Room lists, message history, media and sending, in plain types the UI can show.

use anyhow::{Context, Result, anyhow, bail};
use matrix_sdk::media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings};
use matrix_sdk::room::MessagesOptions;
use matrix_sdk::room::edit::EditedContent;
use matrix_sdk::room::reply::{EnforceThread, Reply};
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::events::room::message::{
    AddMentions, MessageType, Relation, RoomMessageEventContent, RoomMessageEventContentWithoutRelation,
};
use matrix_sdk::ruma::events::room::member::MembershipState;
use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent, SyncMessageLikeEvent, SyncStateEvent};
use matrix_sdk::ruma::{
    EventId, MilliSecondsSinceUnixEpoch, MxcUri, OwnedEventId, OwnedMxcUri, OwnedRoomId, OwnedUserId, RoomId, UInt, UserId,
};
use matrix_sdk::{Client, Room};
use std::collections::HashMap;
use std::path::Path;

/// Shown for messages this device has no keys for yet.
/// Whether a message body is the "can't decrypt" placeholder.
pub fn is_undecryptable(body: &str) -> bool {
    body.starts_with(UNDECRYPTABLE)
}

/// Why a message can't be decrypted, in plain words.
pub fn undecryptable_reason(r: &matrix_sdk::deserialized_responses::UnableToDecryptReason) -> String {
    use matrix_sdk::deserialized_responses::UnableToDecryptReason as R;
    match r {
        R::MissingMegolmSession { withheld_code: None } => "no key for it on this device".into(),
        R::MissingMegolmSession { withheld_code: Some(code) } => format!("the sender's device withheld the key: {code}"),
        R::UnknownMegolmMessageIndex => "this device has the key, but only from a later message on".into(),
        R::MegolmDecryptionFailure => "the key on this device doesn't fit it".into(),
        R::MismatchedIdentityKeys => "the key came from a device that doesn't match the sender".into(),
        R::SenderIdentityNotTrusted(_) => "the sender isn't trusted enough".into(),
        R::MalformedEncryptedEvent => "the message is malformed".into(),
        other => format!("{other:?}"),
    }
}

pub const UNDECRYPTABLE: &str = "Encrypted message — this device doesn't have the key yet";

#[derive(Debug, Clone)]
pub struct RoomSummary {
    pub room_id: OwnedRoomId,
    pub name: String,
    pub invite: bool,
    pub avatar: Option<OwnedMxcUri>,
    pub topic: Option<String>,
    pub encrypted: bool,
    pub unread: u64,
    pub members: u64,
    pub latest: Option<HistoryItem>,
    /// Favourite ("pinned") rooms sort first. Stored as the standard
    /// `m.favourite` tag, so other clients see it too.
    pub pinned: bool,
    /// A space: a folder of rooms rather than a chat.
    pub is_space: bool,
    /// For spaces: the rooms (and spaces) it lists, in its order.
    pub children: Vec<OwnedRoomId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
    Audio,
    File,
}

/// An attachment on a message.
#[derive(Debug, Clone)]
pub struct Media {
    pub kind: MediaKind,
    pub source: MediaSource,
    /// A server-made thumbnail, when the sender provided one.
    pub thumbnail: Option<MediaSource>,
    pub name: String,
    pub mime: Option<String>,
    pub size: Option<u64>,
    pub width: Option<u64>,
    pub height: Option<u64>,
    /// Not sent as a file but as a text message of its link: what it is
    /// isn't known until the server is asked (`files::probe`).
    pub linked: bool,
}

#[derive(Debug, Clone)]
pub struct HistoryItem {
    pub event_id: OwnedEventId,
    pub sender: OwnedUserId,
    pub sender_name: Option<String>,
    pub sender_avatar: Option<OwnedMxcUri>,
    pub body: String,
    pub ts: MilliSecondsSinceUnixEpoch,
    pub reply_to: Option<OwnedEventId>,
    pub media: Option<Media>,
    pub edited: bool,
    /// A room event (join, rename, …) described in `body`, not a message.
    pub notice: bool,
    /// For join/leave notices: whose membership changed and how.
    pub member: Option<MemberChange>,
    /// A reply in a thread: the message that started it.
    pub thread: Option<OwnedEventId>,
    /// Mentions of people in the text: (name as shown, user id).
    pub pills: Vec<(String, String)>,
    /// A warning about who really sent an encrypted message.
    pub shield: Option<Shield>,
}

/// Element's warning shield on an encrypted message, in its lax mode: no
/// nagging about people you never verified, but a word when the sending
/// device isn't verified by its owner, is unknown, or the person's
/// identity changed since you verified them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shield {
    /// Red (the identity changed, or the sender doesn't match) or grey.
    pub red: bool,
    pub message: &'static str,
}

pub fn shield(info: Option<&matrix_sdk::deserialized_responses::EncryptionInfo>) -> Option<Shield> {
    use matrix_sdk::deserialized_responses::ShieldState;
    match info?.verification_state.to_shield_state_lax() {
        ShieldState::Red { message, .. } => Some(Shield { red: true, message }),
        ShieldState::Grey { message, .. } => Some(Shield { red: false, message }),
        ShieldState::None => None,
    }
}

/// Someone's membership after a room event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberChange {
    pub user: OwnedUserId,
    pub joined: bool,
    /// Shown as "joined" only because the server didn't say what came
    /// before: it may just be a name or picture change by someone already in.
    pub guessed: bool,
}

impl HistoryItem {
    pub fn sender_label(&self) -> &str {
        self.sender_name.as_deref().unwrap_or(self.sender.localpart())
    }
}

/// What a message event means for the timeline.
pub enum Parsed {
    /// A new message: body, whether it's real text, reply target, attachment.
    New { body: String, is_text: bool, reply_to: Option<OwnedEventId>, media: Option<Media>, thread: Option<OwnedEventId>, pills: Vec<(String, String)> },
    /// An edit of an earlier message.
    Edit { target: OwnedEventId, body: String },
}

/// Whether a file message's text is a caption, not the stand-in
/// [`message_body`] gives files without one.
pub fn is_caption(body: &str) -> bool {
    !matches!(body, "Sent an image" | "Sent a video" | "Sent audio" | "Sent a sticker" | "")
        && !body.starts_with("Sent a file: ")
        && !is_undecryptable(body)
}

/// Readable text for a message, and whether it is real text (not a label).
pub fn message_body(m: &MessageType) -> (String, bool) {
    match m {
        MessageType::Text(t) => (t.body.clone(), true),
        MessageType::Notice(t) => (t.body.clone(), true),
        MessageType::Emote(t) => (format!("* {}", t.body), true),
        // A caption, when the file has one, shows under it.
        MessageType::Image(c) => (
            c.caption()
                .map_or_else(|| "Sent an image".into(), str::to_owned),
            false,
        ),
        MessageType::Video(c) => (
            c.caption()
                .map_or_else(|| "Sent a video".into(), str::to_owned),
            false,
        ),
        MessageType::Audio(c) => (
            c.caption()
                .map_or_else(|| "Sent audio".into(), str::to_owned),
            false,
        ),
        MessageType::File(f) => (
            f.caption()
                .map_or_else(|| format!("Sent a file: {}", f.body), str::to_owned),
            false,
        ),
        MessageType::Location(_) => ("Shared a location".into(), false),
        other => (other.body().to_owned(), true),
    }
}

/// A sticker someone sent (`m.sticker`): shown as its picture.
pub fn parse_sticker(c: &matrix_sdk::ruma::events::sticker::StickerEventContent) -> Parsed {
    let u = |v: Option<UInt>| v.map(u64::from);
    let (reply_to, thread) = match &c.relates_to {
        Some(Relation::Reply(r)) => (Some(r.in_reply_to.event_id.clone()), None),
        Some(Relation::Thread(t)) => (t.in_reply_to.as_ref().filter(|_| !t.is_falling_back).map(|r| r.event_id.clone()), Some(t.event_id.clone())),
        _ => (None, None),
    };
    let media = Media {
        kind: MediaKind::Image,
        source: c.source.clone().into(),
        thumbnail: c.info.thumbnail_source.clone(),
        name: c.body.clone(),
        mime: c.info.mimetype.clone(),
        size: u(c.info.size),
        width: u(c.info.width),
        height: u(c.info.height),
        linked: false,
    };
    Parsed::New { body: "Sent a sticker".into(), is_text: false, reply_to, media: Some(media), thread, pills: Vec::new() }
}

fn media_of(m: &MessageType) -> Option<Media> {
    let u = |v: Option<UInt>| v.map(u64::from);
    Some(match m {
        MessageType::Image(c) => Media {
            kind: MediaKind::Image,
            source: c.source.clone(),
            thumbnail: c.info.as_ref().and_then(|i| i.thumbnail_source.clone()),
            name: c.filename().to_owned(),
            mime: c.info.as_ref().and_then(|i| i.mimetype.clone()),
            size: c.info.as_ref().and_then(|i| u(i.size)),
            width: c.info.as_ref().and_then(|i| u(i.width)),
            height: c.info.as_ref().and_then(|i| u(i.height)),
            linked: false,
        },
        MessageType::Video(c) => Media {
            kind: MediaKind::Video,
            source: c.source.clone(),
            thumbnail: c.info.as_ref().and_then(|i| i.thumbnail_source.clone()),
            name: c.filename().to_owned(),
            mime: c.info.as_ref().and_then(|i| i.mimetype.clone()),
            size: c.info.as_ref().and_then(|i| u(i.size)),
            width: None,
            height: None,
            linked: false,
        },
        MessageType::Audio(c) => Media {
            kind: MediaKind::Audio,
            source: c.source.clone(),
            thumbnail: None,
            name: c.filename().to_owned(),
            mime: c.info.as_ref().and_then(|i| i.mimetype.clone()),
            size: c.info.as_ref().and_then(|i| u(i.size)),
            width: None,
            height: None,
            linked: false,
        },
        MessageType::File(c) => Media {
            kind: MediaKind::File,
            source: c.source.clone(),
            thumbnail: c.info.as_ref().and_then(|i| i.thumbnail_source.clone()),
            name: c.filename().to_owned(),
            mime: c.info.as_ref().and_then(|i| i.mimetype.clone()),
            size: c.info.as_ref().and_then(|i| u(i.size)),
            width: None,
            height: None,
            linked: false,
        },
        _ => return None,
    })
}

/// A file shown from a message that is only its link; a plain file until
/// its server says what it is.
pub fn linked_media(mxc: OwnedMxcUri) -> Media {
    Media {
        kind: MediaKind::File,
        name: mxc.media_id().unwrap_or("file").to_owned(),
        source: MediaSource::Plain(mxc),
        thumbnail: None,
        mime: None,
        size: None,
        width: None,
        height: None,
        linked: true,
    }
}

/// Drops the "> <@user> quoted text" fallback older clients put at the top of replies.
fn strip_reply_fallback(body: &str) -> String {
    if !body.starts_with("> ") {
        return body.to_owned();
    }
    let mut lines = body.lines().peekable();
    while lines.peek().is_some_and(|l| l.starts_with('>')) {
        lines.next();
    }
    while lines.peek().is_some_and(|l| l.trim().is_empty()) {
        lines.next();
    }
    lines.collect::<Vec<_>>().join("\n")
}

pub fn parse(content: &RoomMessageEventContent) -> Parsed {
    match &content.relates_to {
        Some(Relation::Replacement(r)) => Parsed::Edit { target: r.event_id.clone(), body: message_body(&r.new_content.msgtype).0 },
        rel => {
            let (mut body, is_text) = message_body(&content.msgtype);
            let (reply_to, thread) = match rel {
                Some(Relation::Reply(r)) => (Some(r.in_reply_to.event_id.clone()), None),
                // In a thread, `in_reply_to` is only a real reply when it isn't the fallback.
                Some(Relation::Thread(t)) => (t.in_reply_to.as_ref().filter(|_| !t.is_falling_back).map(|r| r.event_id.clone()), Some(t.event_id.clone())),
                _ => (None, None),
            };
            if reply_to.is_some() || thread.is_some() {
                body = strip_reply_fallback(&body);
            }
            let pills = match &content.msgtype {
                MessageType::Text(t) => t.formatted.as_ref().map(|f| mention_names(&f.body)).unwrap_or_default(),
                MessageType::Emote(t) => t.formatted.as_ref().map(|f| mention_names(&f.body)).unwrap_or_default(),
                MessageType::Notice(t) => t.formatted.as_ref().map(|f| mention_names(&f.body)).unwrap_or_default(),
                _ => Vec::new(),
            };
            let media = media_of(&content.msgtype).or_else(|| match (&content.msgtype, crate::files::parse_link(&body)) {
                (MessageType::Text(_), Some(crate::files::FileLink::Media(mxc))) => Some(linked_media(mxc)),
                _ => None,
            });
            Parsed::New { body, is_text, reply_to, media, thread, pills }
        }
    }
}

pub async fn rooms(client: &Client) -> Vec<RoomSummary> {
    let mut out = Vec::new();
    for (room, invite) in client
        .invited_rooms()
        .into_iter()
        .map(|r| (r, true))
        .chain(client.joined_rooms().into_iter().map(|r| (r, false)))
    {
        if !invite && room.create_content().is_none() {
            let r = room.clone();
            tokio::spawn(async move { check_room_type(&r).await });
        }
        let is_space = is_space(&room);
        out.push(RoomSummary {
            is_space,
            children: if is_space { space_children(&room).await } else { Vec::new() },
            room_id: room.room_id().to_owned(),
            name: room_name(&room).await,
            invite,
            avatar: room.avatar_url(),
            topic: room.topic(),
            encrypted: room.encryption_state().is_encrypted(),
            unread: room.unread_notification_counts().notification_count,
            members: room.joined_members_count(),
            latest: None,
            pinned: room.is_favourite(),
        });
    }
    out
}

/// Rooms known to be spaces although their own state doesn't say so (yet):
/// right after joining, or when the server never sent the creation event.
static SPACES: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<OwnedRoomId>>> = std::sync::LazyLock::new(Default::default);

/// Whether a room is a space (a folder of rooms) rather than a chat.
pub fn is_space(room: &Room) -> bool {
    room.is_space() || SPACES.lock().unwrap().contains(room.room_id())
}

/// Asks the server what kind of room this is, when the local store has no
/// creation event for it (once per room). Spaces are remembered.
pub async fn check_room_type(room: &Room) {
    use matrix_sdk::ruma::api::client::state::get_state_event_for_key::v3::Request;
    use matrix_sdk::ruma::events::StateEventType;
    static ASKED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<OwnedRoomId>>> = std::sync::LazyLock::new(Default::default);
    if room.is_space() || !ASKED.lock().unwrap().insert(room.room_id().to_owned()) {
        return;
    }
    let req = Request::new(room.room_id().to_owned(), StateEventType::RoomCreate, String::new());
    let Ok(resp) = room.client().send(req).await else { return };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(resp.event_or_content.get()) else { return };
    // The content, or (with `format=event`) a whole event holding it.
    let content = v.get("content").filter(|c| c.is_object()).unwrap_or(&v);
    if content.get("type").and_then(|t| t.as_str()) == Some("m.space") {
        SPACES.lock().unwrap().insert(room.room_id().to_owned());
    }
}

/// A room listed in a space, from the server.
#[derive(Debug, Clone)]
pub struct SpaceRoom {
    pub room_id: OwnedRoomId,
    pub name: String,
    pub topic: Option<String>,
    pub members: u64,
    pub avatar: Option<OwnedMxcUri>,
    pub is_space: bool,
    /// Servers that can help us join it.
    pub via: Vec<matrix_sdk::ruma::OwnedServerName>,
}

/// The rooms (and sub-spaces) a space lists, joined or not, in the space's
/// order. Uses the server's space hierarchy; if the server can't give it,
/// the space's own list (names unknown for rooms we aren't in).
pub async fn space_rooms(space: &Room) -> Vec<SpaceRoom> {
    use matrix_sdk::ruma::api::client::space::get_hierarchy::v1::Request;
    let order = read_space_children_via(space).await;
    let mut req = Request::new(space.room_id().to_owned());
    req.max_depth = Some(UInt::from(1u32));
    req.limit = Some(UInt::from(200u32));
    let mut found: HashMap<OwnedRoomId, SpaceRoom> = HashMap::new();
    match space.client().send(req).await {
        Ok(resp) => {
            for chunk in resp.rooms {
                let r = chunk.summary;
                if r.room_id == space.room_id() {
                    continue;
                }
                let is_space = r.room_type == Some(matrix_sdk::ruma::room::RoomType::Space);
                if is_space {
                    SPACES.lock().unwrap().insert(r.room_id.clone());
                }
                let name = r.name.clone().or_else(|| r.canonical_alias.as_ref().map(|a| a.to_string())).unwrap_or_else(|| r.room_id.to_string());
                found.insert(r.room_id.clone(), SpaceRoom { name, topic: r.topic, members: r.num_joined_members.into(), avatar: r.avatar_url, is_space, via: Vec::new(), room_id: r.room_id });
            }
        }
        Err(e) => tracing::debug!(space = %space.room_id(), "no space hierarchy: {e}"),
    }
    let client = space.client();
    let mut out = Vec::new();
    for (id, via) in order {
        let mut r = found.remove(&id).unwrap_or_else(|| {
            // Not in the hierarchy: what we know locally.
            let local = client.get_room(&id);
            SpaceRoom {
                name: local.as_ref().and_then(|r| r.cached_display_name().map(|n| n.to_string())).unwrap_or_else(|| id.to_string()),
                topic: local.as_ref().and_then(|r| r.topic()),
                members: local.as_ref().map_or(0, |r| r.joined_members_count()),
                avatar: local.as_ref().and_then(|r| r.avatar_url()),
                is_space: local.as_ref().is_some_and(is_space),
                via: Vec::new(),
                room_id: id.clone(),
            }
        });
        r.via = via;
        out.push(r);
    }
    // Rooms the hierarchy lists that our copy of the space doesn't yet.
    out.extend(found.into_values());
    out
}

/// Joins a room listed in a space, through the servers the space gave.
pub async fn join_listed(client: &Client, room: &SpaceRoom) -> Result<Room> {
    let mut via = room.via.clone();
    if let Some(s) = room.room_id.server_name() {
        via.push(s.to_owned());
    }
    via.dedup();
    let target: &matrix_sdk::ruma::RoomOrAliasId = (&*room.room_id).into();
    let joined = join_patiently(client, target, &via).await?;
    check_room_type(&joined).await;
    Ok(joined)
}

/// The rooms a space lists, remembered for a little while: the room list
/// is redrawn often and this reads the space's state from the store.
async fn space_children(space: &Room) -> Vec<OwnedRoomId> {
    use std::sync::{LazyLock, Mutex};
    use std::time::{Duration, Instant};
    type Children = (Instant, Vec<OwnedRoomId>);
    static CACHE: LazyLock<Mutex<HashMap<OwnedRoomId, Children>>> = LazyLock::new(Default::default);
    if let Some((t, kids)) = CACHE.lock().unwrap().get(space.room_id())
        && t.elapsed() < Duration::from_secs(20)
    {
        return kids.clone();
    }
    let kids = read_space_children(space).await;
    CACHE.lock().unwrap().insert(space.room_id().to_owned(), (Instant::now(), kids.clone()));
    kids
}

/// The rooms a space lists (`m.space.child` events that still have a
/// `via`; an empty one means the room was taken out), by their `order`.
pub async fn read_space_children(space: &Room) -> Vec<OwnedRoomId> {
    read_space_children_via(space).await.into_iter().map(|(id, _)| id).collect()
}

/// [`read_space_children`] with each room's `via` servers.
async fn read_space_children_via(space: &Room) -> Vec<(OwnedRoomId, Vec<matrix_sdk::ruma::OwnedServerName>)> {
    use matrix_sdk::deserialized_responses::SyncOrStrippedState;
    use matrix_sdk::ruma::events::space::child::SpaceChildEventContent;
    let Ok(events) = space.get_state_events_static::<SpaceChildEventContent>().await else { return Vec::new() };
    type Kid = (Option<String>, MilliSecondsSinceUnixEpoch, OwnedRoomId, Vec<matrix_sdk::ruma::OwnedServerName>);
    let mut kids: Vec<Kid> = events
        .into_iter()
        .filter_map(|raw| match raw.deserialize().ok()? {
            SyncOrStrippedState::Sync(SyncStateEvent::Original(e)) if !e.content.via.is_empty() => {
                let order = e.content.order.as_ref().map(|o| o.as_str().to_owned()).filter(|o| o.len() <= 50 && o.bytes().all(|b| (0x20..=0x7e).contains(&b)));
                Some((order, e.origin_server_ts, e.state_key, e.content.via))
            }
            _ => None,
        })
        .collect();
    // Ordered ones first, by `order`; the rest by when they were added.
    kids.sort_by(|a, b| match (&a.0, &b.0) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.1.cmp(&b.1),
    });
    kids.into_iter().map(|k| (k.2, k.3)).collect()
}

/// The room's name; uses the name cached by the last sync when there is one
/// (computing it from members is slow on big accounts).
pub async fn room_name(room: &Room) -> String {
    if let Some(n) = room.cached_display_name() {
        return n.to_string();
    }
    match room.display_name().await {
        Ok(n) => n.to_string(),
        Err(_) => room.room_id().to_string(),
    }
}

/// Display name and avatar of a room member, from the local store.
pub async fn member_profile(room: &Room, user: &UserId) -> (Option<String>, Option<OwnedMxcUri>) {
    match room.get_member_no_sync(user).await {
        Ok(Some(m)) => (m.display_name().map(str::to_owned), m.avatar_url().map(ToOwned::to_owned)),
        _ => (None, None),
    }
}

/// How far someone has read in a room (their public read receipt).
#[derive(Debug, Clone)]
pub struct ReadMark {
    pub user: OwnedUserId,
    pub name: Option<String>,
    pub avatar: Option<OwnedMxcUri>,
    pub event_id: OwnedEventId,
    pub ts: Option<MilliSecondsSinceUnixEpoch>,
}

/// Everyone else's latest read receipt in `room`, from the local store (no
/// network). Private receipts aren't shared, so those readers don't show.
pub async fn read_marks(room: &Room) -> Vec<ReadMark> {
    use matrix_sdk::ruma::events::receipt::{ReceiptThread, ReceiptType};
    let me = room.own_user_id();
    let Ok(members) = room.members_no_sync(matrix_sdk::RoomMemberships::JOIN).await else { return Vec::new() };
    let mut out = Vec::new();
    for m in members.iter().filter(|m| m.user_id() != me).take(300) {
        let mut best: Option<(OwnedEventId, matrix_sdk::ruma::events::receipt::Receipt)> = None;
        for thread in [ReceiptThread::Unthreaded, ReceiptThread::Main] {
            if let Ok(Some(r)) = room.load_user_receipt(ReceiptType::Read, &thread, m.user_id()).await
                && best.as_ref().is_none_or(|b| b.1.ts < r.1.ts)
            {
                best = Some(r);
            }
        }
        if let Some((event_id, receipt)) = best {
            out.push(ReadMark {
                user: m.user_id().to_owned(),
                name: m.display_name().map(str::to_owned),
                avatar: m.avatar_url().map(ToOwned::to_owned),
                event_id,
                ts: receipt.ts,
            });
        }
    }
    out
}

/// A page of history, oldest first, and the token for the page before it.
pub struct Page {
    pub items: Vec<HistoryItem>,
    /// Emoji reactions seen on this page (to messages on it or elsewhere).
    pub reactions: Vec<Reaction>,
    pub older: Option<String>,
}

/// One person's emoji reaction to a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    /// The reaction event itself (redacting it takes the reaction back).
    pub event_id: OwnedEventId,
    pub target: OwnedEventId,
    pub key: String,
    pub sender: OwnedUserId,
}

/// Messages before `from` (or the newest ones when `from` is `None`). Edits
/// are applied and deleted messages left out.
///
/// Room events (joins, renames, …) come back as notices. Pages holding no
/// messages are skipped over (up to a few requests), and `older` is `None`
/// once the start of the room (or of what we may see) is reached, so
/// "load older" only shows when there is something to load.
pub async fn history(room: &Room, from: Option<String>, limit: u32) -> Result<Page> {
    let mut out = Vec::new();
    let mut reactions = Vec::new();
    let mut from = from;
    let mut saw_create = false;
    for _ in 0..5 {
        let (mut items, mut r, older, created) = history_page(room, from.clone(), limit).await?;
        saw_create |= created;
        reactions.append(&mut r);
        items.append(&mut out);
        out = items;
        let more = older.is_some() && older != from;
        from = older;
        if !more {
            from = None;
            break;
        }
        if out.iter().filter(|i| !i.notice).count() >= (limit as usize / 2).max(1) {
            break;
        }
    }
    // The server stopped before the room's start: it hides what came before
    // we joined (or were invited). Say so, since keys can't bring it back.
    if from.is_none() && !saw_create {
        use matrix_sdk::ruma::events::room::history_visibility::HistoryVisibility;
        let text = match room.history_visibility_or_default() {
            HistoryVisibility::Joined => Some(hidden_history("joined")),
            HistoryVisibility::Invited => Some(hidden_history("were invited")),
            // History is open to members, but the server gave nothing older.
            _ => Some(
                "No older messages came from your server. The room lets members see its history, but your \
                 server couldn't get it from the room's other servers (they may be offline or slow)."
                    .to_owned(),
            ),
        };
        if let (Some(text), Ok(id)) = (text, OwnedEventId::try_from("$whisker-history-start")) {
            let ts = out.first().map_or(0u64, |m| u64::from(m.ts.get()).saturating_sub(1));
            out.insert(
                0,
                HistoryItem {
                    event_id: id,
                    sender: room.own_user_id().to_owned(),
                    sender_name: None,
                    sender_avatar: None,
                    body: text,
                    ts: MilliSecondsSinceUnixEpoch(UInt::new(ts).unwrap_or_default()),
                    reply_to: None,
                    media: None,
                    edited: false,
                    notice: true,
                    member: None,
                    thread: None,
                    pills: Vec::new(),
                    shield: None,
                },
            );
        }
    }
    Ok(Page { items: out, reactions, older: from })
}

/// One `/messages` request: its items, oldest first, and the token for the
/// page before (`None` at the start of the room).
fn hidden_history(when: &str) -> String {
    format!(
        "Earlier messages are hidden: this room only shows members its history from when they {when}. \
         The server doesn't send older messages to you, so importing keys can't bring them back. \
         A room admin can change this (History: all members) for messages sent from now on."
    )
}

/// Also says whether the room's creation event was on it.
async fn history_page(room: &Room, from: Option<String>, limit: u32) -> Result<(Vec<HistoryItem>, Vec<Reaction>, Option<String>, bool)> {
    let mut opts = MessagesOptions::backward();
    opts.limit = UInt::from(limit);
    opts.from = from;
    let msgs = room.messages(opts).await?;
    let mut out = Vec::new();
    let mut edits: HashMap<OwnedEventId, String> = HashMap::new();
    let mut profiles: HashMap<OwnedUserId, (Option<String>, Option<OwnedMxcUri>)> = HashMap::new();
    // No `end` token, or a page with nothing on it, means the server has
    // nothing older: either the start of the room or the start of what we're
    // allowed to see (some servers hand out a token even then). A short page
    // with a token isn't the end: the server may still be fetching older
    // messages from the room's other servers.
    let mut at_start = msgs.end.is_none() || msgs.chunk.is_empty();
    let mut created = false;
    let mut reactions = Vec::new();
    // Newest first: an edit is seen before the message it changes.
    for ev in &msgs.chunk {
        let m = match ev.raw().deserialize() {
            Ok(AnySyncTimelineEvent::MessageLike(m)) => m,
            Ok(AnySyncTimelineEvent::State(_)) => {
                let Some(st) = state_event(ev.raw()) else { continue };
                created |= matches!(st, AnySyncStateEvent::RoomCreate(_));
                at_start |= created;
                if let Some(item) = notice_item(room, &st, &mut profiles).await {
                    out.push(item);
                }
                continue;
            }
            _ => continue,
        };
        let (event_id, sender, ts, parsed) = match m {
            AnySyncMessageLikeEvent::Reaction(SyncMessageLikeEvent::Original(r)) => {
                reactions.push(Reaction { event_id: r.event_id, target: r.content.relates_to.event_id, key: r.content.relates_to.key, sender: r.sender });
                continue;
            }
            AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(m)) => {
                (m.event_id, m.sender, m.origin_server_ts, parse(&m.content))
            }
            AnySyncMessageLikeEvent::Sticker(SyncMessageLikeEvent::Original(m)) => (m.event_id, m.sender, m.origin_server_ts, parse_sticker(&m.content)),
            AnySyncMessageLikeEvent::RoomEncrypted(SyncMessageLikeEvent::Original(m)) => {
                let why = match &ev.kind {
                    matrix_sdk::deserialized_responses::TimelineEventKind::UnableToDecrypt { utd_info, .. } => undecryptable_reason(&utd_info.reason),
                    _ => "unknown reason".to_owned(),
                };
                let body = format!("{UNDECRYPTABLE} ({why})");
                (m.event_id, m.sender, m.origin_server_ts, Parsed::New { body, is_text: false, reply_to: None, media: None, thread: None, pills: Vec::new() })
            }
            _ => continue,
        };
        let (body, reply_to, media, thread, pills) = match parsed {
            Parsed::Edit { target, body } => {
                edits.entry(target).or_insert(body);
                continue;
            }
            Parsed::New { body, reply_to, media, thread, pills, .. } => (body, reply_to, media, thread, pills),
        };
        if !profiles.contains_key(&sender) {
            profiles.insert(sender.clone(), member_profile(room, &sender).await);
        }
        let (sender_name, sender_avatar) = profiles[&sender].clone();
        let edited = edits.remove(&event_id);
        out.push(HistoryItem {
            edited: edited.is_some(),
            body: edited.unwrap_or(body),
            event_id,
            sender,
            sender_name,
            sender_avatar,
            ts,
            reply_to,
            media,
            notice: false,
            member: None,
            thread,
            pills,
            shield: shield(ev.encryption_info().map(|i| &**i)),
        });
    }
    out.reverse();
    Ok((out, reactions, if at_start { None } else { msgs.end }, created))
}

/// A room (state) event from the timeline. Reads `prev_content` from the top
/// of the event too (where older servers put it) when `unsigned` has none:
/// without it, every name or picture change looks like a fresh join.
pub fn state_event(raw: &matrix_sdk::ruma::serde::Raw<AnySyncTimelineEvent>) -> Option<AnySyncStateEvent> {
    let parsed = match raw.deserialize().ok()? {
        AnySyncTimelineEvent::State(st) => st,
        AnySyncTimelineEvent::MessageLike(_) => return None,
    };
    let needs_prev = matches!(&parsed, AnySyncStateEvent::RoomMember(SyncStateEvent::Original(e)) if e.prev_content().is_none());
    if !needs_prev {
        return Some(parsed);
    }
    let mut v: serde_json::Value = serde_json::from_str(raw.json().get()).ok()?;
    let Some(prev) = v.get("prev_content").cloned() else { return Some(parsed) };
    let unsigned = v.as_object_mut()?.entry("unsigned").or_insert_with(|| serde_json::json!({}));
    unsigned.as_object_mut()?.insert("prev_content".into(), prev);
    serde_json::from_value(v).ok().or(Some(parsed))
}

/// A room event as a timeline notice ("Alice joined"), if it's worth one.
pub async fn notice_item(
    room: &Room,
    ev: &AnySyncStateEvent,
    profiles: &mut HashMap<OwnedUserId, (Option<String>, Option<OwnedMxcUri>)>,
) -> Option<HistoryItem> {
    let mut users = vec![ev.sender().to_owned()];
    if let Ok(target) = OwnedUserId::try_from(ev.state_key()) {
        users.push(target);
    }
    for u in &users {
        if !profiles.contains_key(u) {
            profiles.insert(u.clone(), member_profile(room, u).await);
        }
    }
    let body = crate::notices::describe(ev, |id| {
        users
            .iter()
            .find(|u| u.as_str() == id)
            .and_then(|u| profiles.get(u).and_then(|p| p.0.clone()))
            .unwrap_or_else(|| id.trim_start_matches('@').split(':').next().unwrap_or(id).to_owned())
    })?;
    let (sender_name, sender_avatar) = profiles.get(ev.sender()).cloned().unwrap_or_default();
    let member = match ev {
        AnySyncStateEvent::RoomMember(SyncStateEvent::Original(e)) => Some(MemberChange {
            user: e.state_key.clone(),
            joined: e.content.membership == MembershipState::Join,
            guessed: e.content.membership == MembershipState::Join && e.prev_content().is_none(),
        }),
        _ => None,
    };
    Some(HistoryItem {
        event_id: ev.event_id().to_owned(),
        sender: ev.sender().to_owned(),
        sender_name,
        sender_avatar,
        body,
        ts: ev.origin_server_ts(),
        reply_to: None,
        media: None,
        edited: false,
        notice: true,
        member,
        thread: None,
        pills: Vec::new(),
        shield: None,
    })
}

/// One message's sender name and text, asked of the server: for a reply to
/// a message that isn't among the loaded ones.
pub async fn message_text(room: &Room, id: &EventId) -> Result<(String, String)> {
    let ev = room.event(id, None).await?;
    let (sender, body) = match ev.raw().deserialize()? {
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(m))) => {
            let (Parsed::New { body, .. } | Parsed::Edit { body, .. }) = parse(&m.content);
            (m.sender, body)
        }
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::Sticker(SyncMessageLikeEvent::Original(m))) => {
            let (Parsed::New { body, .. } | Parsed::Edit { body, .. }) = parse_sticker(&m.content);
            (m.sender, body)
        }
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomEncrypted(SyncMessageLikeEvent::Original(m))) => (m.sender, UNDECRYPTABLE.to_owned()),
        _ => bail!("that isn't a message"),
    };
    let name = member_profile(room, &sender).await.0.unwrap_or_else(|| sender.localpart().to_owned());
    Ok((name, body))
}

/// Downloads media (decrypting it if needed). With `thumb`, asks the server
/// for a small version; encrypted media has no server thumbnails, so then
/// the full file comes back. Cached in the account's encrypted media store.
pub async fn download(client: &Client, source: &MediaSource, thumb: Option<u32>) -> Result<Vec<u8>> {
    let format = match (source, thumb) {
        (MediaSource::Plain(_), Some(px)) => MediaFormat::Thumbnail(MediaThumbnailSettings::new(UInt::from(px), UInt::from(px))),
        _ => MediaFormat::File,
    };
    let req = MediaRequestParameters { source: source.clone(), format };
    Ok(client.media().get_media_content(&req, true).await?)
}

/// [`download`] of media posted in `room`: if nobody else can give it, it
/// can come from another server in that room (when that's turned on; see
/// [`crate::media_fetch`]).
pub async fn download_in(client: &Client, room: &RoomId, source: &MediaSource, thumb: Option<u32>) -> Result<Vec<u8>> {
    crate::media_fetch::in_room(room.to_owned(), download(client, source, thumb)).await
}

/// A small square thumbnail of an avatar, as encoded image bytes.
pub async fn thumbnail(client: &Client, mxc: &MxcUri, size: u32) -> Result<Vec<u8>> {
    download(client, &MediaSource::Plain(mxc.to_owned()), Some(size)).await
}

/// Sends Markdown text, optionally as a reply.
/// Prefix of the ids we give messages shown before the server has them.
pub const LOCAL_ECHO: &str = "$whisker-local-";

/// Sends a message and returns the id the server gave it.
/// `mentions` are people picked from @-suggestions: (name as typed, user).
/// Their names become links in the formatted text (pills in other apps)
/// while the plain text keeps just the names, and they're listed so they
/// get notified.
pub async fn send_text(room: &Room, text: &str, reply_to: Option<&EventId>, mentions: &[(String, OwnedUserId)]) -> Result<OwnedEventId> {
    let (linked, users) = link_mentions(text, mentions);
    let mut content = RoomMessageEventContentWithoutRelation::text_markdown(&linked);
    if !users.is_empty() {
        if let MessageType::Text(t) = &mut content.msgtype {
            t.body = text.to_owned();
        }
        content = content.add_mentions(matrix_sdk::ruma::events::Mentions::with_user_ids(users));
    }
    let content = match reply_to {
        Some(id) => room
            .make_reply_event(content, Reply { event_id: id.to_owned(), enforce_thread: EnforceThread::MaybeThreaded, add_mentions: AddMentions::Yes })
            .await
            .map_err(|e| anyhow!("couldn't build the reply: {e}"))?,
        None => content.with_relation(None),
    };
    let started = std::time::Instant::now();
    // The SDK retries failed requests forever by default; give up instead,
    // so the message isn't stuck at "Sending…" (retries reuse the
    // transaction id, so they can't post it twice).
    let config = matrix_sdk::config::RequestConfig::default().retry_limit(3).max_retry_time(std::time::Duration::from_secs(20));
    let sent = room.send(content).with_request_config(config).await?;
    tracing::debug!(room = %room.room_id(), ms = started.elapsed().as_millis() as u64, "message sent (incl. encryption setup)");
    Ok(sent.response.event_id)
}

/// `text` with the first use of each mentioned name made a Markdown link to
/// that person, and the people still mentioned.
pub fn link_mentions(text: &str, picked: &[(String, OwnedUserId)]) -> (String, Vec<OwnedUserId>) {
    let mut out = text.to_owned();
    let mut users = Vec::new();
    for (name, user) in picked {
        if users.contains(user) {
            continue;
        }
        if let Some(i) = out.find(name.as_str()) {
            let link = format!("[{}](https://matrix.to/#/{user})", name.replace(['[', ']'], ""));
            out.replace_range(i..i + name.len(), &link);
            users.push(user.clone());
        }
    }
    (out, users)
}

/// Mentions in a formatted message: (shown name, user id) for each of its
/// links to users (`https://matrix.to/#/@…` or `matrix:u/…`).
pub fn mention_names(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(a) = rest.find("<a ") {
        rest = &rest[a..];
        let Some(close) = rest.find('>') else { break };
        let tag = &rest[..close];
        let user = link_user(tag);
        let body = &rest[close + 1..];
        let Some(end) = body.find("</a>") else { break };
        if let Some(user) = user {
            let name = body[..end].replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'");
            let name = name.trim().to_owned();
            if !name.is_empty() && !out.iter().any(|(n, _)| *n == name) {
                out.push((name, user));
            }
        }
        rest = &body[end..];
    }
    out
}

/// The user an `<a href=…>` tag links to, if it's a user link.
fn link_user(tag: &str) -> Option<String> {
    let href = tag.split("href=").nth(1)?;
    let href = href.trim_start_matches(['"', '\'']);
    let href = &href[..href.find(['"', '\'', ' ']).unwrap_or(href.len())];
    let id = if let Some(rest) = href.split("matrix.to/#/").nth(1) {
        rest.split(['?', '/']).next()?.replace("%40", "@").replace("%3A", ":").replace("%3a", ":")
    } else {
        format!("@{}", href.strip_prefix("matrix:u/")?.split('?').next()?)
    };
    (id.starts_with('@') && id.contains(':')).then_some(id)
}

/// Posts `text` as a reply in the thread started by `root`; `latest` is
/// the thread's newest message (for clients without thread support).
pub async fn send_in_thread(room: &Room, root: &EventId, latest: &EventId, text: &str) -> Result<OwnedEventId> {
    use matrix_sdk::ruma::events::relation::Thread;
    let mut content = RoomMessageEventContent::text_markdown(text);
    content.relates_to = Some(Relation::Thread(Thread::plain(root.to_owned(), latest.to_owned())));
    let config = matrix_sdk::config::RequestConfig::default().retry_limit(3).max_retry_time(std::time::Duration::from_secs(20));
    Ok(room.send(content).with_request_config(config).await?.response.event_id)
}

/// Replaces the text of one of our own messages.
pub async fn edit_text(room: &Room, target: &EventId, text: &str) -> Result<()> {
    let new = RoomMessageEventContentWithoutRelation::text_markdown(text);
    let content = room.make_edit_event(target, EditedContent::RoomMessage(new)).await.map_err(|e| anyhow!("couldn't edit: {e}"))?;
    room.send(content).await?;
    Ok(())
}

/// Changes the caption of a picture, video or file you sent (an empty
/// caption removes it). The file stays as it is.
pub async fn edit_caption(room: &Room, target: &EventId, caption: &str) -> Result<()> {
    use matrix_sdk::ruma::events::room::message::FormattedBody;
    let caption = caption.trim();
    let edit = EditedContent::MediaCaption {
        caption: (!caption.is_empty()).then(|| caption.to_owned()),
        formatted_caption: FormattedBody::markdown(caption),
        mentions: None,
    };
    let content = room.make_edit_event(target, edit).await.map_err(|e| anyhow!("couldn't edit: {e}"))?;
    room.send(content).await?;
    Ok(())
}

/// Reacts to a message with `key` (an emoji). Returns the reaction's id.
pub async fn react(room: &Room, target: &EventId, key: &str) -> Result<OwnedEventId> {
    use matrix_sdk::ruma::events::reaction::ReactionEventContent;
    use matrix_sdk::ruma::events::relation::Annotation;
    let content = ReactionEventContent::new(Annotation::new(target.to_owned(), key.to_owned()));
    let config = matrix_sdk::config::RequestConfig::default().retry_limit(3).max_retry_time(std::time::Duration::from_secs(20));
    Ok(room.send(content).with_request_config(config).await?.response.event_id)
}

/// Posts a copy of a message from `from` into `to`. Media is re-posted as
/// the same file (no download or upload); replies and mentions are dropped,
/// and an edited message goes as its newest text.
pub async fn forward(from: &Room, event_id: &EventId, to: &Room, edited_body: Option<&str>) -> Result<OwnedEventId> {
    let content = match edited_body {
        Some(body) => RoomMessageEventContent::text_plain(body),
        None => {
            let ev = from.event(event_id, None).await.context("couldn't fetch that message")?;
            let mut content = match ev.raw().deserialize().context("couldn't read that message")? {
                AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(m))) => m.content,
                AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomEncrypted(_)) => {
                    bail!("this message can't be forwarded: this device doesn't have its key")
                }
                _ => bail!("only messages can be forwarded"),
            };
            let reply = matches!(content.relates_to, Some(Relation::Reply { .. }));
            content.relates_to = None;
            content.mentions = None;
            // A reply carries a quote of what it answers; send just its own text.
            if let (true, MessageType::Text(t)) = (reply, &content.msgtype) {
                content = RoomMessageEventContent::text_plain(strip_reply_fallback(&t.body));
            }
            content
        }
    };
    let config = matrix_sdk::config::RequestConfig::default().retry_limit(3).max_retry_time(std::time::Duration::from_secs(20));
    Ok(to.send(content).with_request_config(config).await?.response.event_id)
}

/// Deletes (redacts) up to `limit` of a room's newest messages, reactions
/// and the like, newest first (`None`: all of them). Room events (joins,
/// names…) and what's already deleted stay. `progress` hears the count so
/// far. Returns (deleted, failed).
///
/// With `only`, just that person's messages are deleted and counted (what
/// someone who can't delete others' messages clears: their own).
pub async fn clear_room(room: &Room, limit: Option<usize>, only: Option<&UserId>, progress: impl Fn(usize)) -> Result<(usize, usize)> {
    let limit = limit.unwrap_or(usize::MAX);
    let (mut done, mut failed) = (0usize, 0usize);
    let mut from: Option<String> = None;
    // Pages in a row with nothing in them (a server that filters by sender
    // after cutting the page can send some before the room's start).
    let mut empty = 0;
    loop {
        let mut opts = MessagesOptions::backward();
        opts.limit = UInt::from(100u32);
        opts.from = from.clone();
        if let Some(who) = only {
            // The server skips everyone else's, so a long room goes quickly.
            opts.filter.senders = Some(vec![who.to_owned()]);
        }
        let page = room.messages(opts).await?;
        for ev in &page.chunk {
            if done + failed >= limit {
                return Ok((done, failed));
            }
            let raw = ev.raw();
            let kind = raw.get_field::<String>("type").ok().flatten().unwrap_or_default();
            let is_state = raw.get_field::<serde_json::Value>("state_key").ok().flatten().is_some();
            let gone = raw.get_field::<serde_json::Value>("unsigned").ok().flatten().is_some_and(|u| u.get("redacted_because").is_some());
            if is_state || gone || kind == "m.room.redaction" {
                continue;
            }
            if let Some(who) = only
                && raw.get_field::<OwnedUserId>("sender").ok().flatten().as_deref() != Some(who)
            {
                continue;
            }
            let Some(id) = ev.event_id() else { continue };
            match room.redact(id, None, None).await {
                Ok(_) => {
                    done += 1;
                    progress(done);
                }
                Err(e) => {
                    tracing::warn!("couldn't delete {id}: {e}");
                    failed += 1;
                }
            }
            // Gentle on the server's rate limits.
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        }
        empty = if page.chunk.is_empty() { empty + 1 } else { 0 };
        let patience = if only.is_some() { 10 } else { 1 };
        match page.end {
            Some(end) if empty < patience && from.as_ref() != Some(&end) => from = Some(end),
            _ => return Ok((done, failed)),
        }
    }
}

/// Most bytes we'll upload in one go, whatever the server allows: the
/// file is held (and encrypted) in memory.
pub const MAX_UPLOAD: u64 = 2 * 1024 * 1024 * 1024;

/// The biggest upload `room`'s server takes (and we do).
async fn upload_limit(room: &Room) -> u64 {
    match room.client().load_or_fetch_max_upload_size().await {
        Ok(n) => u64::from(n).min(MAX_UPLOAD),
        Err(_) => MAX_UPLOAD,
    }
}

fn too_big(limit: u64) -> anyhow::Error {
    anyhow::anyhow!("that file is over {} MB, the most this server takes", limit / 1_048_576)
}

/// Uploads a file from disk (prepared as `opts` say: see [`crate::upload`])
/// and returns the message to post with [`crate::upload::post`]; calls
/// `progress` with the percentage sent as it goes.
pub async fn upload_file(
    room: &Room,
    path: &Path,
    opts: &crate::upload::UploadOptions,
    progress: impl Fn(u8) + Send + 'static,
) -> Result<MessageType> {
    let meta = tokio::fs::metadata(path)
        .await
        .context("can't read that file")?;
    let limit = upload_limit(room).await;
    if meta.len() > limit {
        return Err(too_big(limit));
    }
    let data = tokio::fs::read(path).await?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    upload_bytes(room, &name, data, Some(path.to_owned()), opts, progress).await
}

/// Uploads a file from disk and posts it (as an image when it is one).
pub async fn send_file(
    room: &Room,
    path: &Path,
    opts: &crate::upload::UploadOptions,
    progress: impl Fn(u8) + Send + 'static,
) -> Result<()> {
    let message = upload_file(room, path, opts, progress).await?;
    crate::upload::post(room, message).await.map(drop)
}

/// `path`: where the file is on disk, for a video's preview frame.
async fn upload_bytes(
    room: &Room,
    name: &str,
    data: Vec<u8>,
    path: Option<std::path::PathBuf>,
    opts: &crate::upload::UploadOptions,
    progress: impl Fn(u8) + Send + 'static,
) -> Result<MessageType> {
    let (name, opts2) = (name.to_owned(), opts.clone());
    let prepared = tokio::task::spawn_blocking(move || {
        // A poster frame, so a video shows as a picture rather than a file.
        let poster = path
            .filter(|p| {
                mime_guess::from_path(p)
                    .first()
                    .is_some_and(|m| m.type_() == mime_guess::mime::VIDEO)
            })
            .and_then(|p| video_poster(&p));
        crate::upload::prepare(&name, data, &opts2, poster)
    })
    .await?;
    let limit = upload_limit(room).await;
    if prepared.data.len() as u64 > limit {
        return Err(too_big(limit));
    }
    crate::upload::upload(room, prepared, opts.always_encrypt, progress).await
}

/// A still from a video, made by ffmpeg (when it's installed).
pub struct Poster {
    pub jpeg: Vec<u8>,
    /// The still's width and height.
    pub size: (u32, u32),
    /// The video's.
    pub video_size: (u32, u32),
    pub duration: Option<std::time::Duration>,
}

/// Widest poster frame made.
const POSTER_PX: u32 = 640;

/// What ffprobe says about a video file.
#[derive(Clone, Copy, Debug)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub duration: Option<std::time::Duration>,
    /// It has a sound track.
    pub audio: bool,
    /// Its first subtitle track, if it has one.
    pub subtitles: Option<Subtitles>,
}

/// What kind of subtitles a video carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subtitles {
    /// Text (SRT, ASS, WebVTT, MP4 text), drawn with a font.
    Text,
    /// Pictures (Blu-ray PGS, DVD, DVB), laid over the video.
    Bitmap,
}

fn subtitle_kind(codec: &str) -> Option<Subtitles> {
    match codec {
        "subrip" | "srt" | "ass" | "ssa" | "mov_text" | "webvtt" | "text" | "microdvd" => Some(Subtitles::Text),
        "hdmv_pgs_subtitle" | "dvd_subtitle" | "dvb_subtitle" | "xsub" => Some(Subtitles::Bitmap),
        _ => None,
    }
}

/// Size, length, sound and subtitles, via ffprobe (when installed). `input`
/// is a file, or an address ffprobe can read.
pub fn probe_video(input: impl AsRef<std::ffi::OsStr>) -> Option<VideoInfo> {
    use std::process::{Command, Stdio};
    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_type,codec_name,width,height:stream_side_data=rotation:stream_tags=rotate:format=duration", "-of", "default=nw=1"])
        .arg(input)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    parse_probe(&String::from_utf8_lossy(&probe.stdout))
}

/// Reads ffprobe's `key=value` lines. The size is the one the video is
/// shown at: phones film upright videos sideways and mark them "turn 90°",
/// which players (ffmpeg too) do, so width and height swap.
fn parse_probe(text: &str) -> Option<VideoInfo> {
    let field = |k: &str| text.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix('=').map(str::trim).filter(|v| *v != "N/A"));
    let width: u32 = field("width")?.parse().ok()?;
    let height: u32 = field("height")?.parse().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    let turned = field("rotation").or_else(|| field("TAG:rotate")).and_then(|r| r.parse::<f64>().ok()).is_some_and(|r| (r.round() as i64).rem_euclid(180) == 90);
    let (width, height) = if turned { (height, width) } else { (width, height) };
    let duration = field("duration").and_then(|d| d.parse::<f64>().ok()).filter(|d| d.is_finite() && *d >= 0.0).map(std::time::Duration::from_secs_f64);
    let audio = text.lines().any(|l| l.trim() == "codec_type=audio");
    // Each stream lists its codec, then its type.
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let subtitles = lines.windows(2).find_map(|w| match (w[0].strip_prefix("codec_name="), w[1]) {
        (Some(codec), "codec_type=subtitle") => subtitle_kind(codec),
        _ => None,
    });
    Some(VideoInfo { width, height, duration, audio, subtitles })
}

/// A frame from early in the video at `path` (a file, or an address
/// ffmpeg can read), as a JPEG.
pub fn video_poster(path: impl AsRef<std::ffi::OsStr>) -> Option<Poster> {
    use std::process::{Command, Stdio};
    let path = path.as_ref();
    let VideoInfo { width: w, height: h, duration, .. } = probe_video(path)?;
    let pw = w.min(POSTER_PX) & !1;
    let ph = ((u64::from(h) * u64::from(pw) / u64::from(w)) as u32).max(2) & !1;
    // A second in skips black fade-ins; short clips use their first frame.
    for seek in ["1", "0"] {
        let out = Command::new("ffmpeg")
            .args(["-v", "error", "-ss", seek, "-i"])
            .arg(path)
            .args(["-frames:v", "1", "-vf", &format!("scale={pw}:{ph}"), "-f", "image2", "-c:v", "mjpeg", "-q:v", "4", "pipe:1"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.stdout.is_empty() {
            return Some(Poster { jpeg: out.stdout, size: (pw, ph), video_size: (w, h), duration });
        }
    }
    None
}

/// Uploads `data` as a file called `name` (its extension sets the type)
/// and posts it.
pub async fn send_bytes(
    room: &Room,
    name: &str,
    data: Vec<u8>,
    opts: &crate::upload::UploadOptions,
) -> Result<()> {
    let limit = upload_limit(room).await;
    if data.len() as u64 > limit {
        return Err(too_big(limit));
    }
    let message = upload_bytes(room, name, data, None, opts, |_| ()).await?;
    crate::upload::post(room, message).await.map(drop)
}

#[cfg(test)]
mod tests {
    #[test]
    fn upright_phone_videos_swap_their_size() {
        let probe = |extra: &str| super::parse_probe(&format!("codec_name=h264\ncodec_type=video\nwidth=1920\nheight=1080\n{extra}duration=1.5\n")).unwrap();
        assert_eq!((probe("").width, probe("").height), (1920, 1080));
        assert_eq!((probe("rotation=-90\n").width, probe("rotation=-90\n").height), (1080, 1920));
        assert_eq!((probe("TAG:rotate=270\n").width, probe("TAG:rotate=270\n").height), (1080, 1920));
        assert_eq!((probe("rotation=180\n").width, probe("rotation=180\n").height), (1920, 1080));
    }

    use super::*;

    #[test]
    fn makes_video_posters() {
        let dir = std::env::temp_dir().join(format!("whisker-poster-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("clip.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=1280x720:rate=10:duration=2", "-pix_fmt", "yuv420p"])
            .arg(&video)
            .status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("no ffmpeg; skipping");
            return;
        }
        let p = video_poster(&video).expect("a poster");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(&p.jpeg[..2], [0xFF, 0xD8]);
        assert_eq!(p.size, (640, 360));
        assert_eq!(p.video_size, (1280, 720));
        assert!(p.duration.is_some_and(|d| d.as_secs_f64() > 1.5));
    }

    #[test]
    fn strips_reply_fallback() {
        assert_eq!(strip_reply_fallback("> <@a:b> hi\n> there\n\nmy answer"), "my answer");
        assert_eq!(strip_reply_fallback("no quote"), "no quote");
    }
}

/// A room listed in a server's public room directory.
#[derive(Debug, Clone)]
pub struct PublicRoom {
    pub room_id: OwnedRoomId,
    pub name: String,
    pub alias: Option<String>,
    pub topic: Option<String>,
    pub members: u64,
    pub avatar: Option<OwnedMxcUri>,
}

/// Searches the public room directory of `server` (or the account's own
/// homeserver). Returns the rooms and a token for the next page.
pub async fn public_rooms(client: &Client, server: Option<&str>, search: &str, since: Option<String>) -> Result<(Vec<PublicRoom>, Option<String>)> {
    use matrix_sdk::ruma::api::client::directory::get_public_rooms_filtered::v3::Request;
    use matrix_sdk::ruma::directory::Filter;
    let mut req = Request::new();
    req.limit = Some(UInt::from(30u32));
    req.since = since;
    if let Some(s) = server.map(str::trim).filter(|s| !s.is_empty()) {
        req.server = Some(s.try_into().map_err(|_| anyhow!("“{s}” isn't a server name like matrix.org"))?);
    }
    let mut filter = Filter::new();
    let q = search.trim();
    if !q.is_empty() {
        filter.generic_search_term = Some(q.to_owned());
    }
    req.filter = filter;
    let resp = client.public_rooms_filtered(req).await?;
    let rooms = resp
        .chunk
        .into_iter()
        .map(|r| PublicRoom {
            name: r
                .name
                .clone()
                .or_else(|| r.canonical_alias.as_ref().map(|a| a.to_string()))
                .unwrap_or_else(|| r.room_id.to_string()),
            alias: r.canonical_alias.map(|a| a.to_string()),
            topic: r.topic,
            members: r.num_joined_members.into(),
            avatar: r.avatar_url,
            room_id: r.room_id,
        })
        .collect();
    Ok((rooms, resp.next_batch))
}

/// Joins a room by id (`!abc:server`) or address (`#name:server`).
pub async fn join(client: &Client, id_or_alias: &str) -> Result<Room> {
    use matrix_sdk::ruma::{OwnedRoomOrAliasId, OwnedServerName};
    let target: OwnedRoomOrAliasId = id_or_alias.trim().try_into().map_err(|_| anyhow!("that isn't a room address like #room:server"))?;
    // Ask the room's own server to help us in, as well as ours.
    let via: Vec<OwnedServerName> = target.server_name().map(ToOwned::to_owned).into_iter().collect();
    let room = join_patiently(client, &target, &via).await?;
    check_room_type(&room).await;
    Ok(room)
}

/// How long a join that timed out is waited for.
const JOIN_WAIT: std::time::Duration = std::time::Duration::from_secs(300);

/// Joins as the SDK does, but waits past the request's time limit. Joining a
/// room on another server can take minutes while ours fetches the room from
/// it, and the join still goes through after the request gave up: then the
/// room turns up as joined in a later sync.
pub async fn join_patiently(client: &Client, target: &matrix_sdk::ruma::RoomOrAliasId, via: &[matrix_sdk::ruma::OwnedServerName]) -> Result<Room> {
    let err = match client.join_room_by_id_or_alias(target, via).await {
        Ok(room) => return Ok(room),
        Err(e) if e.to_string().contains("timed out") => e,
        Err(e) => return Err(e.into()),
    };
    let id = match <&RoomId>::try_from(target) {
        Ok(id) => id.to_owned(),
        Err(alias) => client.resolve_room_alias(alias).await?.room_id,
    };
    tracing::debug!("joining {id} timed out; waiting for the server to finish");
    let started = std::time::Instant::now();
    while started.elapsed() < JOIN_WAIT {
        if let Some(room) = client.get_room(&id).filter(|r| r.state() == matrix_sdk::RoomState::Joined) {
            return Ok(room);
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    Err(anyhow!("{err} (your server may still finish joining; the room shows up if it does)"))
}
