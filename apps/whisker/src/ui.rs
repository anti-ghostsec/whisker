//! UI-thread side: the long-lived list models and image caches.
//!
//! The lists are created once and updated in place (append, replace one
//! row, remove one row) instead of being rebuilt on every event, which keeps
//! scrolling smooth and stops the scroll position jumping.

use crate::format::{self, Kind};
use crate::{AccountItem, AppWindow, Block, ForwardItem, MessageItem, ReactionChip, Reader, RoomItem};
use matrix_sdk::ruma::OwnedMxcUri;
use slint::{Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use whisker_core::timeline::{HistoryItem, MediaKind, UNDECRYPTABLE};

pub type Pixels = SharedPixelBuffer<Rgba8Pixel>;
/// An animation: its frames, and how long each shows (ms).
pub type Frames = (Arc<Vec<Pixels>>, u32);

/// Most frames kept of one animation (the rest are dropped).
const MAX_FRAMES: usize = 150;

/// The frames of an animated GIF or WebP, shrunk to fit; `None` for still
/// pictures (and anything that isn't one of those).
pub fn decode_frames(bytes: &[u8], max_w: u32, max_h: u32) -> Option<Frames> {
    use image::AnimationDecoder;
    let cursor = std::io::Cursor::new(bytes);
    let frames: Vec<image::Frame> = if bytes.starts_with(b"GIF8") {
        image::codecs::gif::GifDecoder::new(cursor).ok()?.into_frames().take(MAX_FRAMES).collect::<Result<_, _>>().ok()?
    } else if bytes.get(8..12) == Some(b"WEBP") {
        let d = image::codecs::webp::WebPDecoder::new(cursor).ok()?;
        if !d.has_animation() {
            return None;
        }
        d.into_frames().take(MAX_FRAMES).collect::<Result<_, _>>().ok()?
    } else {
        return None;
    };
    if frames.len() < 2 {
        return None;
    }
    let (num, den) = frames[0].delay().numer_denom_ms();
    // Browsers treat very short delays as 100 ms; so do we.
    let ms = num.checked_div(den).filter(|ms| *ms > 10).unwrap_or(100);
    let out: Vec<Pixels> = frames
        .into_iter()
        .map(|f| {
            let img = image::DynamicImage::ImageRgba8(f.into_buffer());
            let img = if img.width() > max_w || img.height() > max_h { img.resize(max_w, max_h, image::imageops::FilterType::Triangle) } else { img };
            let img = img.to_rgba8();
            Pixels::clone_from_slice(img.as_raw(), img.width(), img.height())
        })
        .collect();
    Some((Arc::new(out), ms))
}
pub type PixelCache = Arc<Mutex<HashMap<OwnedMxcUri, Option<Pixels>>>>;

pub struct Models {
    pub accounts: Rc<VecModel<AccountItem>>,
    pub rooms: Rc<VecModel<RoomItem>>,
    pub messages: Rc<VecModel<MessageItem>>,
    /// The thread open in the side panel.
    pub thread: Rc<VecModel<MessageItem>>,
    /// Rooms offered by the "Forward to…" dialog.
    pub forward: Rc<VecModel<ForwardItem>>,
    avatar_images: HashMap<OwnedMxcUri, Image>,
    /// Built message rows by event id, with a fingerprint of what they were
    /// built from. Reusing an unchanged row keeps it equal to the one on
    /// screen, so redrawing a room only touches rows that really changed.
    built: HashMap<String, (u64, MessageItem)>,
}

thread_local! {
    static MODELS: RefCell<Option<Models>> = const { RefCell::new(None) };
}

/// Creates the models and hands them to the window. Call once, on the UI thread.
pub fn install(w: &AppWindow) {
    let m = Models {
        accounts: Rc::new(VecModel::default()),
        rooms: Rc::new(VecModel::default()),
        messages: Rc::new(VecModel::default()),
        thread: Rc::new(VecModel::default()),
        forward: Rc::new(VecModel::default()),
        avatar_images: HashMap::new(),
        built: HashMap::new(),
    };
    w.set_accounts(ModelRc::from(m.accounts.clone()));
    w.set_rooms(ModelRc::from(m.rooms.clone()));
    w.set_messages(ModelRc::from(m.messages.clone()));
    w.set_forward_rooms(ModelRc::from(m.forward.clone()));
    w.set_thread_messages(ModelRc::from(m.thread.clone()));
    MODELS.with(|c| *c.borrow_mut() = Some(m));
}

pub fn with<R>(f: impl FnOnce(&mut Models) -> R) -> Option<R> {
    MODELS.with(|c| c.borrow_mut().as_mut().map(f))
}

impl Models {
    /// The avatar image for `mxc`, converted from cached pixels once.
    pub fn avatar(&mut self, mxc: &Option<OwnedMxcUri>, pixels: &PixelCache) -> Image {
        let Some(m) = mxc else { return Image::default() };
        if let Some(i) = self.avatar_images.get(m) {
            return i.clone();
        }
        match pixels.lock().unwrap().get(m) {
            Some(Some(p)) => {
                let img = Image::from_rgba8(p.clone());
                self.avatar_images.insert(m.clone(), img.clone());
                img
            }
            _ => Image::default(),
        }
    }

    pub fn clear(&mut self) {
        self.accounts.set_vec(Vec::new());
        self.rooms.set_vec(Vec::new());
        self.messages.set_vec(Vec::new());
        self.thread.set_vec(Vec::new());
        self.forward.set_vec(Vec::new());
        self.avatar_images.clear();
        self.built.clear();
    }
}

/// Replaces a list's rows, touching only rows that changed when the shape
/// is the same (so the list doesn't reset its scroll position).
pub fn sync_rows<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>, same_key: impl Fn(&T, &T) -> bool) {
    if model.row_count() == rows.len() && rows.iter().enumerate().all(|(i, r)| model.row_data(i).is_some_and(|o| same_key(&o, r))) {
        for (i, r) in rows.into_iter().enumerate() {
            if model.row_data(i).as_ref() != Some(&r) {
                model.set_row_data(i, r);
            }
        }
    } else {
        model.set_vec(rows);
    }
}

pub fn index_of(model: &VecModel<MessageItem>, id: &str) -> Option<usize> {
    (0..model.row_count()).rev().find(|&i| model.row_data(i).is_some_and(|m| m.id == id))
}

pub fn s(x: impl AsRef<str>) -> SharedString {
    x.as_ref().into()
}

/// First letter (skipping a sigil) and a stable colour index.
pub fn initial_hue(name: &str) -> (SharedString, i32) {
    let trimmed = name.trim_start_matches(['@', '!', '#']);
    let initial = trimmed.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".into());
    let hue = name.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32)) % 8;
    (initial.into(), hue as i32)
}

thread_local! {
    /// Rich text (styled, not selectable) instead of selectable plain text.
    pub static RICH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `pills` are mentions (name, user id): lines naming them are drawn styled
/// (even when rich text is off) with the name as a link, like Element's pills.
pub fn blocks(body: &str, pills: &[(String, String)]) -> ModelRc<Block> {
    let lines = if RICH.get() { format::split(body) } else { format::merged(body) };
    let rich = RICH.get();
    let v: Vec<Block> = lines
        .into_iter()
        .map(|l| {
            let text_like = matches!(l.kind, Kind::Text | Kind::Quote | Kind::Heading);
            let mention = text_like && pills.iter().any(|(n, _)| l.source.contains(n.as_str()));
            Block {
                kind: l.kind as i32,
                mention,
                text: match l.kind {
                    Kind::Code | Kind::Blank => Default::default(),
                    _ if mention => format::styled(&format::link_mentions(&l.source, pills)),
                    _ if !rich => Default::default(),
                    _ => format::styled(&l.source),
                },
                plain: s(&l.source),
            }
        })
        .collect();
    ModelRc::new(VecModel::from(v))
}

/// Everything about one message row that isn't a Slint type, so it can be
/// prepared off the UI thread.
#[derive(Clone)]
pub struct MsgRow {
    pub item: HistoryItem,
    pub mine: bool,
    pub first: bool,
    pub day: String,
    pub reply: Option<(String, String)>,
    pub picture: Option<Pixels>,
    /// Animated pictures: all frames.
    pub frames: Option<Frames>,
    /// People whose read receipt lands on this message: (name, avatar).
    pub readers: Vec<(String, Option<OwnedMxcUri>)>,
    /// Emoji reactions: (emoji, how many, one of them is ours, who).
    pub reactions: Vec<(String, usize, bool, String)>,
    /// For a thread's first message: replies, and the newest one ("Bob: hi").
    pub thread: Option<(usize, String)>,
}

pub fn human_size(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.0} KB", n as f64 / 1024.0),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

/// Size an image preview to fit 360×300, keeping its shape.
pub fn picture_size(w: Option<u64>, h: Option<u64>, px: Option<&Pixels>) -> (f32, f32) {
    let (w, h) = match (w, h, px) {
        (Some(w), Some(h), _) if w > 0 && h > 0 => (w as f32, h as f32),
        (_, _, Some(p)) => (p.width() as f32, p.height() as f32),
        _ => (320.0, 200.0),
    };
    let scale = (360.0 / w).min(300.0 / h).min(1.0);
    ((w * scale).max(60.0), (h * scale).max(40.0))
}

impl Models {
    /// The row for `r`, reused from last time when nothing about it changed.
    pub fn message(&mut self, r: &MsgRow, pixels: &PixelCache) -> MessageItem {
        use std::hash::{Hash, Hasher};
        let loaded = |m: &Option<OwnedMxcUri>| m.as_ref().is_some_and(|m| matches!(pixels.lock().unwrap().get(m), Some(Some(_))));
        let mut h = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", r.item).hash(&mut h);
        (r.mine, r.first, &r.day, &r.reply, &r.readers, &r.reactions, &r.thread, RICH.get()).hash(&mut h);
        r.picture.as_ref().map(|p| (p.width(), p.height())).hash(&mut h);
        r.frames.as_ref().map(|f| (f.0.len(), f.1)).hash(&mut h);
        loaded(&r.item.sender_avatar).hash(&mut h);
        r.readers.iter().map(|x| loaded(&x.1)).collect::<Vec<_>>().hash(&mut h);
        let sig = h.finish();
        let id = r.item.event_id.to_string();
        if let Some((_, item)) = self.built.get(&id).filter(|(old, _)| *old == sig) {
            return item.clone();
        }
        if self.built.len() > 4000 {
            self.built.clear();
        }
        let item = self.build_message(r, pixels);
        self.built.insert(id, (sig, item.clone()));
        item
    }

    fn build_message(&mut self, r: &MsgRow, pixels: &PixelCache) -> MessageItem {
        let name = r.item.sender_label().to_owned();
        let (initial, hue) = initial_hue(&name);
        // Shown before the server has it (see `Ctl::send`).
        let local = r.item.event_id.as_str().starts_with(whisker_core::timeline::LOCAL_ECHO);
        let (media, media_name, media_info, (pw, ph)) = match &r.item.media {
            Some(m) => (
                match m.kind {
                    MediaKind::Image => 1,
                    MediaKind::Video => 2,
                    MediaKind::Audio => 3,
                    MediaKind::File => 4,
                },
                m.name.clone(),
                [m.mime.clone(), m.size.map(human_size)].into_iter().flatten().collect::<Vec<_>>().join(" · "),
                picture_size(m.width, m.height, r.picture.as_ref()),
            ),
            None => (0, String::new(), String::new(), (0.0, 0.0)),
        };
        let (reply_name, reply_text) = r.reply.clone().unwrap_or_default();
        MessageItem {
            id: s(r.item.event_id.as_str()),
            notice: r.item.notice,
            sender: s(r.item.sender.as_str()),
            name: s(&name),
            body: s(&r.item.body),
            blocks: blocks(&r.item.body, &r.item.pills),
            links: ModelRc::new(VecModel::from(format::links(&r.item.body).iter().map(s).collect::<Vec<_>>())),
            readers: ModelRc::new(VecModel::from(
                r.readers
                    .iter()
                    .take(5)
                    .map(|(name, avatar)| {
                        let (initial, hue) = initial_hue(name);
                        Reader { name: s(name), initial, hue, avatar: self.avatar(avatar, pixels) }
                    })
                    .collect::<Vec<_>>(),
            )),
            more_readers: r.readers.len().saturating_sub(5) as i32,
            thread_replies: r.thread.as_ref().map_or(0, |t| t.0 as i32),
            thread_last: s(r.thread.as_ref().map(|t| t.1.as_str()).unwrap_or_default()),
            reactions: ModelRc::new(VecModel::from(
                r.reactions.iter().map(|(key, n, mine, who)| ReactionChip { key: s(key), count: *n as i32, mine: *mine, who: s(who) }).collect::<Vec<_>>(),
            )),
            mine: r.mine,
            time: if local { s("Sending…") } else { s(crate::fmt_time(r.item.ts)) },
            initial,
            hue,
            avatar: self.avatar(&r.item.sender_avatar, pixels),
            first: r.first,
            day: s(&r.day),
            faint: local || r.item.body == UNDECRYPTABLE,
            edited: r.item.edited,
            reply_name: s(reply_name),
            reply_text: s(reply_text),
            media,
            media_name: s(media_name),
            media_info: s(media_info),
            picture: r.picture.as_ref().map(|p| Image::from_rgba8(p.clone())).unwrap_or_default(),
            frames: ModelRc::new(VecModel::from(
                r.frames.as_ref().map(|f| f.0.iter().map(|p| Image::from_rgba8(p.clone())).collect::<Vec<_>>()).unwrap_or_default(),
            )),
            frame_ms: r.frames.as_ref().map_or(0, |f| f.1 as i32),
            picture_width: pw,
            picture_height: ph,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gifs_animate() {
        use image::codecs::gif::{GifEncoder, Repeat};
        let mut bytes = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut bytes);
            enc.set_repeat(Repeat::Infinite).unwrap();
            for shade in [0u8, 255] {
                let img = image::RgbaImage::from_pixel(40, 20, image::Rgba([shade, 0, 0, 255]));
                enc.encode_frame(image::Frame::from_parts(img, 0, 0, image::Delay::from_numer_denom_ms(50, 1))).unwrap();
            }
        }
        let (frames, ms) = decode_frames(&bytes, 20, 20).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(ms, 50);
        assert_eq!((frames[0].width(), frames[0].height()), (20, 10));
        // A still picture isn't an animation.
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbaImage::new(4, 4).write_to(&mut png, image::ImageFormat::Png).unwrap();
        assert!(decode_frames(png.get_ref(), 20, 20).is_none());
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(10), "10 B");
        assert_eq!(human_size(2048), "2 KB");
        assert_eq!(picture_size(Some(1920), Some(1080), None), (360.0, 202.5));
        assert_eq!(picture_size(Some(100), Some(50), None), (100.0, 50.0));
    }
}
