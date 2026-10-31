//! One event stream for every logged-in account.

use crate::profile::AccountId;
use crate::timeline::Media;
use matrix_sdk::ruma::{MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedMxcUri, OwnedRoomId, OwnedUserId};

#[derive(Debug, Clone)]
pub enum ClientEvent {
    /// An account was added, removed, relabelled, or a vault locked/unlocked.
    AccountsChanged,
    /// A message arrived in a room one of our accounts is in. `body` is
    /// readable text (images, files and undecryptable messages get a label).
    Message {
        account: AccountId,
        room_id: OwnedRoomId,
        event_id: OwnedEventId,
        sender: OwnedUserId,
        sender_name: Option<String>,
        sender_avatar: Option<OwnedMxcUri>,
        body: String,
        origin_server_ts: MilliSecondsSinceUnixEpoch,
        /// False for undecryptable messages and media labels: the AI ignores those.
        is_text: bool,
        /// An `m.notice`: by Matrix convention bot output, which bots never answer.
        is_notice: bool,
        reply_to: Option<OwnedEventId>,
        media: Option<Box<Media>>,
        /// A reply in a thread: the thread's first message.
        thread: Option<OwnedEventId>,
    },
    /// A room event (join, leave, rename, …) as a notice line.
    Notice { account: AccountId, item: Box<crate::timeline::HistoryItem>, room_id: OwnedRoomId },
    /// Someone's read receipt in a room moved.
    Receipts { account: AccountId, room_id: OwnedRoomId },
    /// A message was edited; `body` is its new text.
    Edited { account: AccountId, room_id: OwnedRoomId, target: OwnedEventId, body: String },
    /// Someone reacted to a message with an emoji.
    Reaction { account: AccountId, room_id: OwnedRoomId, reaction: crate::timeline::Reaction },
    /// Who is typing in a room changed (never includes ourselves).
    Typing { account: AccountId, room_id: OwnedRoomId, users: Vec<OwnedUserId> },
    /// A message was deleted.
    Redacted { account: AccountId, room_id: OwnedRoomId, target: OwnedEventId },
    /// A verification started, moved on or ended.
    Verification { account: AccountId, user: OwnedUserId, flow_id: String, state: crate::verify::VerifyState },
    /// A sync response arrived: rooms, names or unread counts may have changed.
    Synced { account: AccountId },
    AccountError {
        account: AccountId,
        message: String,
    },
}
