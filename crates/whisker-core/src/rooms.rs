//! Creating rooms, room details, members and profile editing.

use crate::timeline::{self, PublicRoom};
use anyhow::{Context, Result, anyhow, bail};
use matrix_sdk::ruma::api::client::room::Visibility;
use matrix_sdk::ruma::api::client::room::create_room::v3::{Request as CreateRequest, RoomPreset};
use matrix_sdk::ruma::events::room::encryption::RoomEncryptionEventContent;
use matrix_sdk::ruma::events::room::history_visibility::HistoryVisibility;
use matrix_sdk::ruma::events::room::join_rules::JoinRule;
use matrix_sdk::ruma::events::room::power_levels::UserPowerLevel;
use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent, EmptyStateKey, InitialStateEvent, SyncMessageLikeEvent};
use matrix_sdk::ruma::{OwnedMxcUri, OwnedRoomId, OwnedUserId, UserId};
use matrix_sdk::{Client, Room, RoomMemberships};
use serde::Deserialize;

/// Turns what people type for a server ("https://matrix.org/", "@me:x.org",
/// "matrix.org") into a bare server name.
pub fn normalize_server(input: &str) -> Option<String> {
    let mut s = input.trim();
    for p in ["https://", "http://"] {
        s = s.strip_prefix(p).unwrap_or(s);
    }
    let s = s.split('/').next().unwrap_or_default();
    // An MXID or alias: keep the part after the first ':'.
    let s = if s.starts_with(['@', '#', '!']) { s.split_once(':').map(|(_, h)| h).unwrap_or(s) } else { s };
    (!s.is_empty()).then(|| s.to_lowercase())
}

/// Searches a room directory. Asks our own homeserver first (it fetches
/// other servers' directories over federation). Many servers refuse that,
/// so for another server we then ask that server directly; that request
/// is anonymous but does reveal our IP to it (unless a proxy is set).
pub async fn public_rooms(client: &Client, server: &str, search: &str, since: Option<String>) -> Result<(Vec<PublicRoom>, Option<String>)> {
    let server = normalize_server(server);
    let own = client.homeserver().host_str().map(str::to_lowercase);
    let via_own = timeline::public_rooms(client, server.as_deref(), search, since.clone()).await;
    match (via_own, server) {
        (Ok(r), _) => Ok(r),
        (Err(e), Some(s)) if Some(&s) != own.as_ref() && client.user_id().is_none_or(|u| u.server_name().as_str() != s) => {
            directly(client, &s, search, since)
                .await
                .map_err(|e2| anyhow!("your server couldn't list {s}'s rooms ({e:#}), and {s} didn't answer directly either ({e2:#})"))
        }
        (Err(e), _) => Err(e),
    }
}

#[derive(Deserialize)]
struct WellKnown {
    #[serde(rename = "m.homeserver")]
    homeserver: Option<WellKnownHs>,
}
#[derive(Deserialize)]
struct WellKnownHs {
    base_url: String,
}
#[derive(Deserialize)]
struct DirPage {
    #[serde(default)]
    chunk: Vec<DirRoom>,
    next_batch: Option<String>,
}
#[derive(Deserialize)]
struct DirRoom {
    room_id: OwnedRoomId,
    name: Option<String>,
    canonical_alias: Option<String>,
    topic: Option<String>,
    #[serde(default)]
    num_joined_members: u64,
    avatar_url: Option<OwnedMxcUri>,
}

pub(crate) fn scheme_for(server: &str) -> &'static str {
    let host = server.split(':').next().unwrap_or_default();
    if matches!(host, "localhost" | "127.0.0.1" | "[::1]") { "http" } else { "https" }
}

/// Lists a remote server's public rooms with its client API, anonymously.
async fn directly(client: &Client, server: &str, search: &str, since: Option<String>) -> Result<(Vec<PublicRoom>, Option<String>)> {
    let http = client.http_client();
    let origin = format!("{}://{server}", scheme_for(server));
    let base = match http.get(format!("{origin}/.well-known/matrix/client")).send().await {
        Ok(r) if r.status().is_success() => r
            .json::<WellKnown>()
            .await
            .ok()
            .and_then(|w| w.homeserver)
            .map(|h| h.base_url.trim_end_matches('/').to_owned())
            .unwrap_or_else(|| origin.clone()),
        _ => origin.clone(),
    };
    // GET doesn't need a login (POST, which can search, does), so search
    // within each page here.
    let mut url = format!("{base}/_matrix/client/v3/publicRooms?limit=100");
    if let Some(s) = since {
        url.push_str(&format!("&since={}", urlencode(&s)));
    }
    let resp = http.get(url).send().await.context("no answer")?;
    if !resp.status().is_success() {
        bail!("it answered {}", resp.status());
    }
    let page: DirPage = resp.json().await.context("unexpected answer")?;
    let q = search.trim().to_lowercase();
    let rooms = page
        .chunk
        .into_iter()
        .filter(|r| {
            q.is_empty()
                || [&r.name, &r.canonical_alias, &r.topic].iter().any(|f| f.as_ref().is_some_and(|t| t.to_lowercase().contains(&q)))
        })
        .map(|r| PublicRoom {
            name: r.name.clone().or_else(|| r.canonical_alias.clone()).unwrap_or_else(|| r.room_id.to_string()),
            alias: r.canonical_alias,
            topic: r.topic,
            members: r.num_joined_members,
            avatar: r.avatar_url,
            room_id: r.room_id,
        })
        .collect();
    Ok((rooms, page.next_batch))
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub struct NewRoom {
    pub name: String,
    pub topic: String,
    /// Listed in the directory and joinable by anyone.
    pub public: bool,
    /// Local part of the address for public rooms: "cats" → #cats:server.
    pub alias: String,
    pub encrypted: bool,
    pub invite: Vec<OwnedUserId>,
}

pub fn parse_users(s: &str) -> Result<Vec<OwnedUserId>> {
    s.split([',', ' ', '\n'])
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(|u| u.try_into().map_err(|_| anyhow!("“{u}” isn't a user id like @name:server")))
        .collect()
}

pub async fn create_room(client: &Client, r: NewRoom) -> Result<Room> {
    if r.name.trim().is_empty() {
        bail!("give the room a name");
    }
    let mut req = CreateRequest::new();
    req.name = Some(r.name.trim().to_owned());
    req.topic = Some(r.topic.trim().to_owned()).filter(|t| !t.is_empty());
    req.invite = r.invite;
    if r.public {
        req.preset = Some(RoomPreset::PublicChat);
        req.visibility = Visibility::Public;
        let alias = r.alias.trim().trim_start_matches('#').split(':').next().unwrap_or_default().to_owned();
        if !alias.is_empty() {
            req.room_alias_name = Some(alias);
        }
    } else {
        req.preset = Some(RoomPreset::PrivateChat);
    }
    if r.encrypted {
        req.initial_state = vec![InitialStateEvent::new(EmptyStateKey, RoomEncryptionEventContent::with_recommended_defaults()).to_raw_any()];
    }
    client.create_room(req).await.map_err(|e| anyhow!("couldn't create the room: {e}"))
}

#[derive(Debug, Clone, Default)]
pub struct RoomDetails {
    pub name: String,
    pub topic: String,
    pub alias: String,
    pub avatar: Option<OwnedMxcUri>,
    pub encrypted: bool,
    pub join_rule: String,
    pub history: String,
    pub members: u64,
    pub can_edit: bool,
    /// May change the room's picture.
    pub can_set_avatar: bool,
    pub can_invite: bool,
    /// Text of pinned messages (where rooms usually keep their rules).
    pub pinned: Vec<(String, String)>,
}

pub async fn details(room: &Room) -> RoomDetails {
    let me = room.own_user_id().to_owned();
    let (can_edit, can_set_avatar, can_invite) = match room.power_levels().await {
        Ok(pl) => (
            pl.user_can_send_state(&me, matrix_sdk::ruma::events::StateEventType::RoomTopic),
            pl.user_can_send_state(&me, matrix_sdk::ruma::events::StateEventType::RoomAvatar),
            pl.user_can_invite(&me),
        ),
        Err(_) => (false, false, false),
    };
    let join_rule = match room.join_rule() {
        Some(JoinRule::Public) => "Anyone can join",
        Some(JoinRule::Invite) => "Invite only",
        Some(JoinRule::Knock) => "Ask to join",
        Some(JoinRule::Restricted(_)) | Some(JoinRule::KnockRestricted(_)) => "Members of a space can join",
        _ => "Private",
    };
    let history = match room.history_visibility_or_default() {
        HistoryVisibility::WorldReadable => "History: anyone, even without joining",
        HistoryVisibility::Shared => "History: all members, including before they joined",
        HistoryVisibility::Invited => "History: members, from when they were invited",
        HistoryVisibility::Joined => "History: members, from when they joined",
        _ => "",
    };
    let mut pinned = Vec::new();
    for id in room.pinned_event_ids().unwrap_or_default().into_iter().rev().take(5) {
        let Ok(ev) = room.event(&id, None).await else { continue };
        if let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(m)))) = ev.raw().deserialize() {
            let who = timeline::member_profile(room, &m.sender).await.0.unwrap_or_else(|| m.sender.localpart().to_owned());
            pinned.push((who, timeline::message_body(&m.content.msgtype).0));
        }
    }
    RoomDetails {
        name: timeline::room_name(room).await,
        topic: topic(room).await,
        alias: room
            .canonical_alias()
            .map(|a| a.to_string())
            .unwrap_or_default(),
        avatar: room.avatar_url(),
        encrypted: room.encryption_state().is_encrypted(),
        join_rule: join_rule.into(),
        history: history.into(),
        members: room.joined_members_count(),
        can_edit,
        can_set_avatar,
        can_invite,
        pinned,
    }
}

/// The room's description. When the usual reading has none (a description
/// the SDK couldn't read, or one not synced yet), reads the description
/// event itself: from the store, then from the server.
pub async fn topic(room: &Room) -> String {
    use matrix_sdk::ruma::events::StateEventType;
    if let Some(t) = room.topic().filter(|t| !t.trim().is_empty()) {
        return t;
    }
    if let Ok(Some(raw)) = room.get_state_event(StateEventType::RoomTopic, "").await {
        let json = match &raw {
            matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState::Sync(r) => {
                r.json().get().to_owned()
            }
            matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState::Stripped(r) => {
                r.json().get().to_owned()
            }
        };
        if let Some(t) = serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .and_then(|v| topic_text(&v))
        {
            return t;
        }
    }
    let req = matrix_sdk::ruma::api::client::state::get_state_event_for_key::v3::Request::new(
        room.room_id().to_owned(),
        StateEventType::RoomTopic,
        String::new(),
    );
    match tokio::time::timeout(std::time::Duration::from_secs(10), room.client().send(req)).await {
        Ok(Ok(resp)) => serde_json::from_str::<serde_json::Value>(resp.event_or_content.get())
            .ok()
            .and_then(|v| topic_text(&v))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// The text of a description event, or of its content: the plain `topic`,
/// else the plain (or first) text of the richer `m.topic`, in either the
/// current shape or the older list one.
fn topic_text(v: &serde_json::Value) -> Option<String> {
    let content = v.get("content").filter(|c| c.is_object()).unwrap_or(v);
    if let Some(t) = content
        .get("topic")
        .and_then(|t| t.as_str())
        .filter(|t| !t.trim().is_empty())
    {
        return Some(t.to_owned());
    }
    let block = content.get("m.topic")?;
    let texts = block.get("m.text").unwrap_or(block).as_array()?;
    let pick = texts
        .iter()
        .find(|t| {
            t.get("mimetype")
                .and_then(|m| m.as_str())
                .is_none_or(|m| m == "text/plain")
        })
        .or(texts.first())?;
    let body = pick.get("body")?.as_str()?;
    let plain = if pick.get("mimetype").and_then(|m| m.as_str()) == Some("text/html") {
        html_to_text(body)
    } else {
        body.to_owned()
    };
    Some(plain).filter(|t| !t.trim().is_empty())
}

/// Rough plain text of a bit of HTML: tags dropped, line breaks kept.
fn html_to_text(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let Some(j) = rest[i..].find('>') else {
            rest = "";
            break;
        };
        let tag = rest[i + 1..i + j]
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(
            tag.as_str(),
            "br" | "p"
                | "div"
                | "li"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "tr"
                | "blockquote"
        ) && !out.ends_with('\n')
            && !out.is_empty()
        {
            out.push('\n');
        }
        rest = &rest[i + j + 1..];
    }
    out.push_str(rest);
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .trim()
        .to_owned()
}

#[derive(Debug, Clone)]
pub struct Member {
    pub user_id: OwnedUserId,
    pub name: String,
    pub avatar: Option<OwnedMxcUri>,
    pub level: i64,
}

/// Joined members, admins and moderators first. Fetches the full list from
/// the server the first time (sync only sends the members it needs).
pub async fn members(room: &Room) -> Result<Vec<Member>> {
    let mut out: Vec<Member> = room
        .members(RoomMemberships::JOIN)
        .await?
        .into_iter()
        .map(|m| Member {
            name: m.display_name().map(str::to_owned).unwrap_or_else(|| m.user_id().localpart().to_owned()),
            avatar: m.avatar_url().map(ToOwned::to_owned),
            level: match m.normalized_power_level() {
                UserPowerLevel::Int(i) => i.into(),
                _ => i64::MAX,
            },
            user_id: m.user_id().to_owned(),
        })
        .collect();
    out.sort_by(|a, b| b.level.cmp(&a.level).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(out)
}

pub fn role_name(level: i64) -> String {
    match level {
        i64::MAX => "Creator".into(),
        100.. => "Admin".into(),
        50..=99 => "Moderator".into(),
        1..=49 => format!("Power {level}"),
        _ => String::new(),
    }
}

pub async fn invite(room: &Room, users: &str) -> Result<usize> {
    let users = parse_users(users)?;
    if users.is_empty() {
        bail!("enter at least one user id");
    }
    for u in &users {
        room.invite_user_by_id(u).await.map_err(|e| anyhow!("couldn't invite {u}: {e}"))?;
    }
    Ok(users.len())
}

pub async fn edit(room: &Room, name: &str, topic: &str) -> Result<()> {
    let current = timeline::room_name(room).await;
    if !name.trim().is_empty() && name.trim() != current {
        room.set_name(name.trim().to_owned()).await?;
    }
    if topic.trim() != room.topic().unwrap_or_default().trim() {
        room.set_room_topic(topic.trim()).await?;
    }
    Ok(())
}

/// Sets our display name on the server (visible to everyone we share a room with).
pub async fn set_display_name(client: &Client, name: &str) -> Result<()> {
    let name = name.trim();
    client.account().set_display_name(Some(name).filter(|n| !n.is_empty())).await?;
    Ok(())
}

/// Uploads a picture and makes it our avatar; `None` removes it.
pub async fn set_avatar(client: &Client, image: Option<&std::path::Path>) -> Result<()> {
    match image {
        Some(p) => {
            let data = tokio::fs::read(p).await.context("can't read that file")?;
            let mime = mime_guess::from_path(p).first_or_octet_stream();
            if mime.type_() != "image" {
                bail!("pick an image file");
            }
            client.account().upload_avatar(&mime, data).await?;
        }
        None => client.account().set_avatar_url(None).await?,
    }
    Ok(())
}

/// Makes picture bytes (PNG, JPEG, …) our avatar.
pub async fn set_avatar_data(client: &Client, data: Vec<u8>) -> Result<()> {
    let mime = match data.get(..4) {
        Some([0x89, b'P', b'N', b'G']) => mime_guess::mime::IMAGE_PNG,
        Some([0xff, 0xd8, ..]) => mime_guess::mime::IMAGE_JPEG,
        _ => bail!("the picture must be a PNG or JPEG"),
    };
    client.account().upload_avatar(&mime, data).await?;
    Ok(())
}

/// Uploads a picture and makes it the room's icon; `None` removes it.
pub async fn set_room_avatar(room: &Room, image: Option<&std::path::Path>) -> Result<()> {
    match image {
        Some(p) => {
            let data = tokio::fs::read(p).await.context("can't read that file")?;
            let mime = mime_guess::from_path(p).first_or_octet_stream();
            if mime.type_() != "image" {
                bail!("pick an image file");
            }
            room.upload_avatar(&mime, data, None).await?;
        }
        None => {
            room.remove_avatar().await?;
        }
    }
    Ok(())
}

pub fn is_me(client: &Client, user: &UserId) -> bool {
    client.user_id() == Some(user)
}

/// Accepts an invitation. If the plain join fails (the server picks which
/// remote server to join through, and that one may hand back an incomplete
/// room history), retries naming other servers to join through: the
/// inviter's, the room's own and those of members we know of.
pub async fn accept_invite(room: &Room) -> Result<()> {
    let first = match room.join().await {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    let own = room.own_user_id().server_name().to_owned();
    let mut via: Vec<matrix_sdk::ruma::OwnedServerName> = Vec::new();
    if let Ok(invite) = room.invite_details().await {
        via.push(invite.inviter_id.server_name().to_owned());
    }
    if let Some(s) = room.room_id().server_name() {
        via.push(s.to_owned());
    }
    if let Ok(members) = room.members_no_sync(RoomMemberships::JOIN).await {
        via.extend(members.iter().map(|m| m.user_id().server_name().to_owned()));
    }
    let mut seen = std::collections::BTreeSet::new();
    via.retain(|s| *s != own && seen.insert(s.clone()));
    via.truncate(5);
    let err = if via.is_empty() {
        first
    } else {
        match room.client().join_room_by_id_or_alias(room.room_id().into(), &via).await {
            Ok(_) => return Ok(()),
            Err(e) => e,
        }
    };
    let msg = err.to_string();
    if msg.contains("auth event") || msg.contains("M_NOT_FOUND") {
        bail!(
            "Your homeserver couldn't join: it failed to process the room's history from the other servers \
             (tried {}). This is a problem on your server, not in Whisker; other apps on this account will \
             hit it too. Server said: {msg}",
            if via.is_empty() { "its default".to_owned() } else { via.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ") }
        );
    }
    Err(err.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_servers() {
        assert_eq!(normalize_server("https://Matrix.org/"), Some("matrix.org".into()));
        assert_eq!(normalize_server("@me:tchncs.de"), Some("tchncs.de".into()));
        assert_eq!(normalize_server("#room:envs.net"), Some("envs.net".into()));
        assert_eq!(normalize_server("  "), None);
        assert_eq!(normalize_server("127.0.0.1:8008"), Some("127.0.0.1:8008".into()));
    }

    #[test]
    fn reads_descriptions_in_every_shape() {
        let plain = serde_json::json!({"content": {"topic": "Be nice"}});
        assert_eq!(topic_text(&plain).as_deref(), Some("Be nice"));
        let current = serde_json::json!({"topic": "", "m.topic": {"m.text": [{"body": "<p>Rule 1</p><p>Rule &amp; 2</p>", "mimetype": "text/html"}]}});
        assert_eq!(topic_text(&current).as_deref(), Some("Rule 1\nRule & 2"));
        let older = serde_json::json!({"m.topic": [{"body": "<b>x</b>", "mimetype": "text/html"}, {"body": "Plain rules"}]});
        assert_eq!(topic_text(&older).as_deref(), Some("Plain rules"));
        assert_eq!(topic_text(&serde_json::json!({"topic": " "})), None);
    }

    #[test]
    fn parses_users() {
        assert_eq!(parse_users("@a:x.org, @b:y.org").unwrap().len(), 2);
        assert!(parse_users("bob").is_err());
    }
}

/// A room seen from outside, before joining: what it is, and its newest
/// messages when anyone may read them ("world readable" history).
#[derive(Debug, Clone)]
pub struct Preview {
    pub room_id: OwnedRoomId,
    pub name: String,
    pub alias: Option<String>,
    pub topic: Option<String>,
    pub avatar: Option<OwnedMxcUri>,
    pub members: u64,
    pub is_space: bool,
    /// Anyone may read its history, so `messages` could be fetched.
    pub readable: bool,
    /// Its messages are end-to-end encrypted: only members can read them.
    pub encrypted: bool,
    /// Servers to join through.
    pub via: Vec<matrix_sdk::ruma::OwnedServerName>,
    /// Oldest first.
    pub messages: Vec<timeline::HistoryItem>,
    pub reactions: Vec<timeline::Reaction>,
    /// Why there are no messages, when there aren't.
    pub note: Option<String>,
}

/// Looks at a room (`!id:server` or `#alias:server`) without joining it.
pub async fn preview(client: &Client, id_or_alias: &str, mut via: Vec<matrix_sdk::ruma::OwnedServerName>) -> Result<Preview> {
    use matrix_sdk::ruma::OwnedRoomOrAliasId;
    let target: OwnedRoomOrAliasId = id_or_alias.trim().try_into().map_err(|_| anyhow!("that isn't a room address like #room:server"))?;
    if let Some(s) = target.server_name() {
        via.push(s.to_owned());
    }
    via.dedup();
    let summary = client.get_room_preview(&target, via.clone()).await.context("the server wouldn't describe that room")?;
    let room_id = summary.room_id.clone();
    let name = summary.name.clone().or_else(|| summary.canonical_alias.as_ref().map(|a| a.to_string())).unwrap_or_else(|| room_id.to_string());
    let mut p = Preview {
        name,
        alias: summary.canonical_alias.map(|a| a.to_string()),
        topic: summary.topic,
        avatar: summary.avatar_url,
        members: summary.num_joined_members,
        is_space: matches!(summary.room_type, Some(matrix_sdk::ruma::room::RoomType::Space)),
        readable: summary.is_world_readable.unwrap_or(false),
        encrypted: false,
        via,
        messages: Vec::new(),
        reactions: Vec::new(),
        note: None,
        room_id,
    };
    // Some servers don't say whether it's readable: just try.
    match preview_messages(client, &p.room_id).await {
        Ok((messages, reactions, encrypted)) => {
            p.readable = true;
            p.encrypted = encrypted;
            p.messages = messages;
            p.reactions = reactions;
            if p.messages.is_empty() {
                p.note = Some(if encrypted { "Its messages are encrypted: only members can read them.".into() } else { "No messages yet.".into() });
            }
        }
        Err(e) => {
            tracing::debug!("no preview of {}'s messages: {e:#}", p.room_id);
            p.readable = false;
            p.note = Some("Only members can read this room's messages. Join to see them.".into());
        }
    }
    Ok(p)
}

/// The newest messages of a room we're not in, if its history is open to
/// anyone. Encrypted ones are left out; the flag says there were some.
async fn preview_messages(client: &Client, room_id: &matrix_sdk::ruma::RoomId) -> Result<(Vec<timeline::HistoryItem>, Vec<timeline::Reaction>, bool)> {
    use matrix_sdk::ruma::api::client::filter::{LazyLoadOptions, RoomEventFilter};
    use matrix_sdk::ruma::api::client::message::get_message_events::v3::Request;
    use matrix_sdk::ruma::events::room::member::RoomMemberEventContent;
    use matrix_sdk::ruma::events::{AnyStateEvent, StateEvent};
    let mut req = Request::backward(room_id.to_owned());
    req.limit = matrix_sdk::ruma::UInt::from(50u32);
    let mut filter = RoomEventFilter::default();
    filter.lazy_load_options = LazyLoadOptions::Enabled { include_redundant_members: false };
    req.filter = filter;
    let res = client.send(req).await?;
    // Names and pictures of the people in the page.
    let mut people = std::collections::HashMap::new();
    for raw in &res.state {
        if let Ok(AnyStateEvent::RoomMember(StateEvent::Original(m))) = raw.deserialize() {
            let RoomMemberEventContent { displayname, avatar_url, .. } = m.content;
            if let Ok(user) = OwnedUserId::try_from(m.state_key.as_str()) {
                people.insert(user, (displayname, avatar_url));
            }
        }
    }
    let (mut out, mut reactions, mut edits, mut encrypted) = (Vec::new(), Vec::new(), std::collections::HashMap::new(), false);
    // Newest first: an edit comes before the message it changes.
    for raw in &res.chunk {
        // Read as a sync event: the room is known, and some servers leave out `room_id`.
        let Ok(AnySyncTimelineEvent::MessageLike(ev)) = raw.deserialize_as_unchecked::<AnySyncTimelineEvent>() else { continue };
        match ev {
            AnySyncMessageLikeEvent::RoomEncrypted(_) => encrypted = true,
            AnySyncMessageLikeEvent::Reaction(SyncMessageLikeEvent::Original(r)) => {
                reactions.push(timeline::Reaction { event_id: r.event_id, target: r.content.relates_to.event_id, key: r.content.relates_to.key, sender: r.sender });
            }
            AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(m)) => match timeline::parse(&m.content) {
                timeline::Parsed::Edit { target, body } => {
                    edits.entry(target).or_insert(body);
                }
                timeline::Parsed::New { body, reply_to, media, thread, pills, .. } => {
                    let (sender_name, sender_avatar) = people.get(&m.sender).cloned().unwrap_or_default();
                    let edited = edits.remove(&m.event_id);
                    out.push(timeline::HistoryItem {
                        edited: edited.is_some(),
                        body: edited.unwrap_or(body),
                        event_id: m.event_id,
                        sender: m.sender,
                        sender_name,
                        sender_avatar,
                        ts: m.origin_server_ts,
                        reply_to,
                        media,
                        notice: false,
                        member: None,
                        thread,
                        pills,
                        shield: None,
                    });
                }
            },
            _ => {}
        }
    }
    out.reverse();
    Ok((out, reactions, encrypted))
}
