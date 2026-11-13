//! Getting a file ready to send, and sending it, following the upload
//! settings (chosen once in Settings, not asked each time):
//! - photo metadata (EXIF: often where it was taken, the camera or phone,
//!   when) removed, without re-encoding when it can be;
//! - the file named after a hash of its contents instead of its own name;
//! - pictures converted to another format;
//! - a small preview made for big pictures, so others don't download the
//!   whole picture to show it;
//! - files encrypted even in rooms without encryption, so the server's
//!   file store only holds scrambled data (anyone in the room can still
//!   open it: the key is in the message).
//!
//! Uploading and posting are separate so several files can upload at once
//! and still be posted in the order they were picked.

use anyhow::{Context, Result};
use image::{DynamicImage, ImageDecoder, ImageFormat};
use matrix_sdk::Room;
use matrix_sdk::config::RequestConfig;
use matrix_sdk::ruma::events::room::message::{
    AudioInfo, AudioMessageEventContent, FileInfo, FileMessageEventContent,
    ImageMessageEventContent, MessageType, RoomMessageEventContent, VideoInfo,
    VideoMessageEventContent,
};
use matrix_sdk::ruma::events::room::{ImageInfo, MediaSource, ThumbnailInfo};
use matrix_sdk::ruma::{OwnedEventId, UInt};
use mime_guess::Mime;
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::time::Duration;

/// What pictures are converted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Convert {
    /// Keep the picture's own format.
    #[default]
    Keep,
    Jpeg,
    Png,
}

/// What previews of big pictures are made as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThumbFormat {
    #[default]
    Jpeg,
    Png,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UploadOptions {
    /// Name files after a hash of their contents.
    pub hash_names: bool,
    /// Remove photo metadata (EXIF, XMP, IPTC, text chunks).
    pub strip_metadata: bool,
    pub convert: Convert,
    /// JPEG quality when converting or re-encoding (1-100).
    pub quality: u8,
    /// Make previews for big pictures.
    pub thumbnails: bool,
    pub thumb_format: ThumbFormat,
    /// JPEG quality of previews (1-100).
    pub thumb_quality: u8,
    /// Encrypt files in rooms without encryption too.
    pub always_encrypt: bool,
}

impl Default for UploadOptions {
    fn default() -> Self {
        Self {
            hash_names: true,
            strip_metadata: true,
            convert: Convert::Keep,
            quality: 90,
            thumbnails: true,
            thumb_format: ThumbFormat::Jpeg,
            thumb_quality: 70,
            always_encrypt: false,
        }
    }
}

/// Previews fit in this box (what other apps use too).
const THUMB_BOX: (u32, u32) = (800, 600);

/// A small picture shown in place of the file.
#[derive(Debug, Clone)]
pub struct Thumb {
    pub data: Vec<u8>,
    pub mime: Mime,
    pub size: (u32, u32),
}

/// A file ready to upload.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub name: String,
    pub mime: Mime,
    pub data: Vec<u8>,
    /// A picture's or video's width and height.
    pub size: Option<(u32, u32)>,
    pub duration: Option<Duration>,
    pub thumb: Option<Thumb>,
    /// Why the metadata couldn't be removed, when that was asked for. The
    /// file isn't sent then (see [`crate::timeline`]): sending it as it is
    /// would quietly send what the setting promises to remove.
    pub unclean: Option<String>,
}

/// Makes `data` (called `name`) ready to send. Slow for big pictures: run it
/// off the async threads. `poster`: a video's preview frame, already made.
pub fn prepare(
    name: &str,
    data: Vec<u8>,
    opts: &UploadOptions,
    poster: Option<crate::timeline::Poster>,
) -> Prepared {
    let mut mime = mime_guess::from_path(name).first_or_octet_stream();
    let (mut data, mut size, mut thumb, mut duration) = (data, None, None, None);
    let mut unclean = None;
    if let Some(format) = picture_format(&data) {
        let p = prepare_picture(data, format, opts);
        data = p.0;
        mime = p.1;
        size = p.2;
        thumb = p.3;
        if p.4 {
            unclean = Some("its metadata couldn't be removed".to_owned());
        }
    } else {
        if opts.strip_metadata && let Some(kind) = carries_metadata(name, &mime) {
            unclean = Some(format!("Whisker can't remove the metadata (author, camera, place, history) that {kind} files can carry"));
        }
        let av = matches!(
            mime.type_(),
            mime_guess::mime::VIDEO | mime_guess::mime::AUDIO
        );
        if av && opts.strip_metadata {
            match strip_av(&data, name, mime.type_() == mime_guess::mime::VIDEO) {
                Some(clean) => data = clean,
                None => {
                    unclean = Some(if crate::timeline::ffmpeg_available() {
                        "ffmpeg couldn't read it to remove its metadata".to_owned()
                    } else {
                        "removing a video's or sound's metadata needs ffmpeg, which isn't installed".to_owned()
                    });
                }
            }
        }
        if let Some(p) = poster {
            size = Some(p.video_size);
            duration = p.duration;
            thumb = Some(Thumb {
                data: p.jpeg,
                mime: mime_guess::mime::IMAGE_JPEG,
                size: p.size,
            });
        }
    }
    let name = if opts.hash_names {
        hashed_name(&data, &mime, name)
    } else {
        name.to_owned()
    };
    Prepared {
        name,
        mime,
        data,
        size,
        duration,
        thumb,
        unclean,
    }
}

/// Kinds of file that can carry personal metadata Whisker can't take out
/// (as a word for the message), by name and type. Pictures Whisker can
/// read are cleaned instead, and videos and sound go through ffmpeg.
fn carries_metadata(name: &str, mime: &Mime) -> Option<String> {
    let ext = std::path::Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let known = [
        ("heic", "HEIC"), ("heif", "HEIF"), ("avif", "AVIF"), ("jxl", "JPEG XL"), ("svg", "SVG"),
        ("dng", "RAW"), ("cr2", "RAW"), ("cr3", "RAW"), ("nef", "RAW"), ("arw", "RAW"), ("orf", "RAW"), ("rw2", "RAW"), ("raf", "RAW"),
        ("pdf", "PDF"), ("doc", "Word"), ("docx", "Word"), ("xls", "Excel"), ("xlsx", "Excel"), ("ppt", "PowerPoint"), ("pptx", "PowerPoint"),
        ("odt", "OpenDocument"), ("ods", "OpenDocument"), ("odp", "OpenDocument"), ("rtf", "RTF"), ("epub", "EPUB"),
    ];
    if let Some((_, kind)) = known.iter().find(|(e, _)| *e == ext) {
        return Some((*kind).to_owned());
    }
    // Any other picture type this can't read.
    (mime.type_() == mime_guess::mime::IMAGE).then(|| ext.to_ascii_uppercase()).filter(|e| !e.is_empty())
}

/// Picture formats we can work on.
fn picture_format(data: &[u8]) -> Option<ImageFormat> {
    match image::guess_format(data).ok()? {
        // TIFF and BMP can't be cleaned in place: they're redrawn as PNG.
        f @ (ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP | ImageFormat::Gif | ImageFormat::Tiff | ImageFormat::Bmp) => {
            Some(f)
        }
        _ => None,
    }
}

fn format_mime(f: ImageFormat) -> Mime {
    match f {
        ImageFormat::Jpeg => mime_guess::mime::IMAGE_JPEG,
        ImageFormat::Png => mime_guess::mime::IMAGE_PNG,
        ImageFormat::Gif => mime_guess::mime::IMAGE_GIF,
        ImageFormat::Tiff => "image/tiff".parse().expect("valid"),
        ImageFormat::Bmp => mime_guess::mime::IMAGE_BMP,
        _ => "image/webp".parse().expect("valid"),
    }
}

/// (bytes, type, width and height, preview, metadata left in although
/// asked to remove it).
type Picture = (Vec<u8>, Mime, Option<(u32, u32)>, Option<Thumb>, bool);

fn prepare_picture(
    data: Vec<u8>,
    format: ImageFormat,
    opts: &UploadOptions,
) -> Picture {
    let animated = is_animated(&data, format);
    // Decoded once, only when something needs the pixels.
    let decoded = || decode_upright(&data, format);
    let target = match opts.convert {
        Convert::Jpeg if format != ImageFormat::Jpeg => Some(ImageFormat::Jpeg),
        Convert::Png if format != ImageFormat::Png => Some(ImageFormat::Png),
        _ => None,
    }
    .filter(|_| !animated);
    let mut img: Option<DynamicImage> = None;
    let mut out: Option<(Vec<u8>, ImageFormat)> = None;
    if let Some(to) = target
        && let Some(im) = decoded()
        // JPEG has no transparency: keep see-through pictures as they are.
        && !(to == ImageFormat::Jpeg && has_transparency(&im))
        && let Some(bytes) = encode(&im, to, opts.quality)
    {
        out = Some((bytes, to));
        img = Some(im);
    }
    if out.is_none() && opts.strip_metadata {
        // A turned photo has to be re-encoded upright: the turn is metadata.
        let turned = format == ImageFormat::Jpeg
            && jpeg_orientation(&data)
                .is_some_and(|o| o != image::metadata::Orientation::NoTransforms);
        let stripped = if turned { None } else { strip(&data, format) };
        out = match stripped {
            Some(s) => Some((s, format)),
            None if !animated => decoded().and_then(|im| {
                let to = if format == ImageFormat::Jpeg {
                    ImageFormat::Jpeg
                } else {
                    ImageFormat::Png
                };
                let bytes = encode(&im, to, opts.quality)?;
                img = Some(im);
                Some((bytes, to))
            }),
            None => None,
        };
    }
    let unclean = out.is_none() && opts.strip_metadata;
    let (bytes, format) = out.unwrap_or((data, format));
    let size = img.as_ref().map(|i| (i.width(), i.height())).or_else(|| {
        image::ImageReader::with_format(Cursor::new(&bytes), format)
            .into_dimensions()
            .ok()
    });
    let big = size.is_some_and(|(w, h)| w > THUMB_BOX.0 || h > THUMB_BOX.1);
    let thumb = (opts.thumbnails && big && !animated)
        .then(|| img.or_else(|| decode_upright(&bytes, format)))
        .flatten()
        .and_then(|im| make_thumb(&im, opts));
    (bytes, format_mime(format), size, thumb, unclean)
}

fn decode_upright(data: &[u8], format: ImageFormat) -> Option<DynamicImage> {
    let mut dec = image::ImageReader::with_format(Cursor::new(data), format)
        .into_decoder()
        .ok()?;
    let orientation = dec
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(dec).ok()?;
    img.apply_orientation(orientation);
    Some(img)
}

fn jpeg_orientation(data: &[u8]) -> Option<image::metadata::Orientation> {
    image::codecs::jpeg::JpegDecoder::new(Cursor::new(data))
        .ok()?
        .orientation()
        .ok()
}

fn has_transparency(img: &DynamicImage) -> bool {
    img.color().has_alpha() && img.to_rgba8().pixels().any(|p| p.0[3] < 255)
}

fn encode(img: &DynamicImage, to: ImageFormat, quality: u8) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    match to {
        ImageFormat::Jpeg => {
            let enc =
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100));
            DynamicImage::ImageRgb8(img.to_rgb8())
                .write_with_encoder(enc)
                .ok()?;
        }
        _ => img
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .ok()?,
    }
    Some(out)
}

fn make_thumb(img: &DynamicImage, opts: &UploadOptions) -> Option<Thumb> {
    let small = img.resize(
        THUMB_BOX.0,
        THUMB_BOX.1,
        image::imageops::FilterType::Triangle,
    );
    // A see-through picture's preview stays see-through.
    let to = if opts.thumb_format == ThumbFormat::Png || has_transparency(&small) {
        ImageFormat::Png
    } else {
        ImageFormat::Jpeg
    };
    let data = encode(&small, to, opts.thumb_quality)?;
    Some(Thumb {
        data,
        mime: format_mime(to),
        size: (small.width(), small.height()),
    })
}

fn is_animated(data: &[u8], format: ImageFormat) -> bool {
    match format {
        ImageFormat::Gif => true,
        ImageFormat::WebP => riff_chunks(data).is_some_and(|c| {
            c.iter()
                .any(|(id, body)| id == b"VP8X" && body.first().is_some_and(|f| f & 0x02 != 0))
        }),
        ImageFormat::Png => png_chunks(data).is_some_and(|c| c.iter().any(|(id, _)| id == b"acTL")),
        _ => false,
    }
}

/// The picture without its metadata, the pixels untouched. `None` when it
/// can't be read that way.
pub fn strip(data: &[u8], format: ImageFormat) -> Option<Vec<u8>> {
    match format {
        ImageFormat::Jpeg => strip_jpeg(data),
        ImageFormat::Png => strip_png(data),
        ImageFormat::WebP => strip_webp(data),
        ImageFormat::Gif => strip_gif(data),
        _ => None,
    }
}

/// Keeps what's needed to show the picture (JFIF, the colour profile, the
/// Adobe colour marker) and drops EXIF, XMP, IPTC, comments, maker data and
/// anything after the end (phones put more pictures there, with their own
/// EXIF).
fn strip_jpeg(d: &[u8]) -> Option<Vec<u8>> {
    if !d.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut out = vec![0xFF, 0xD8];
    let mut i = 2;
    loop {
        if *d.get(i)? != 0xFF {
            return None;
        }
        let m = *d.get(i + 1)?;
        match m {
            0xFF => i += 1,
            0xD9 => {
                out.extend_from_slice(&[0xFF, 0xD9]);
                return Some(out);
            }
            0x01 | 0xD0..=0xD7 => {
                out.extend_from_slice(&d[i..i + 2]);
                i += 2;
            }
            _ => {
                let len = u16::from_be_bytes([*d.get(i + 2)?, *d.get(i + 3)?]) as usize;
                let seg = d.get(i..i + 2 + len).filter(|_| len >= 2)?;
                let body = &seg[4..];
                let keep = match m {
                    0xE0 => body.starts_with(b"JFIF\0") || body.starts_with(b"JFXX\0"),
                    0xE2 => body.starts_with(b"ICC_PROFILE\0"),
                    0xEE => body.starts_with(b"Adobe"),
                    0xE1 | 0xE3..=0xED | 0xEF | 0xFE => false,
                    _ => true,
                };
                if keep {
                    out.extend_from_slice(seg);
                }
                i += 2 + len;
                if m == 0xDA {
                    // The picture data, up to the next marker.
                    let start = i;
                    while i + 1 < d.len()
                        && !(d[i] == 0xFF && d[i + 1] != 0 && !(0xD0..=0xD7).contains(&d[i + 1]))
                    {
                        i += 1;
                    }
                    if i + 1 >= d.len() {
                        return None;
                    }
                    out.extend_from_slice(&d[start..i]);
                }
            }
        }
    }
}

const PNG_SIG: &[u8] = b"\x89PNG\r\n\x1a\n";

fn png_chunks(d: &[u8]) -> Option<Vec<([u8; 4], &[u8])>> {
    let mut i = PNG_SIG.len();
    if !d.starts_with(PNG_SIG) {
        return None;
    }
    let mut out = Vec::new();
    loop {
        let len = u32::from_be_bytes(d.get(i..i + 4)?.try_into().ok()?) as usize;
        let id: [u8; 4] = d.get(i + 4..i + 8)?.try_into().ok()?;
        let whole = d.get(i..i.checked_add(12 + len)?)?;
        out.push((id, whole));
        i += 12 + len;
        if &id == b"IEND" {
            return Some(out);
        }
    }
}

/// Drops text (often the program, author or a description), EXIF and
/// the time it was saved; stops at the end.
fn strip_png(d: &[u8]) -> Option<Vec<u8>> {
    let mut out = PNG_SIG.to_vec();
    for (id, whole) in png_chunks(d)? {
        if !matches!(&id, b"tEXt" | b"zTXt" | b"iTXt" | b"eXIf" | b"tIME") {
            out.extend_from_slice(whole);
        }
    }
    Some(out)
}

/// A WebP's chunks: (id, body).
fn riff_chunks(d: &[u8]) -> Option<Vec<([u8; 4], &[u8])>> {
    if d.len() < 12 || &d[..4] != b"RIFF" || &d[8..12] != b"WEBP" {
        return None;
    }
    let end = (u32::from_le_bytes(d[4..8].try_into().ok()?) as usize)
        .checked_add(8)?
        .min(d.len());
    let mut i = 12;
    let mut out = Vec::new();
    while i + 8 <= end {
        let id: [u8; 4] = d[i..i + 4].try_into().ok()?;
        let len = u32::from_le_bytes(d[i + 4..i + 8].try_into().ok()?) as usize;
        let body = d.get(i + 8..(i + 8).checked_add(len)?)?;
        out.push((id, body));
        i += 8 + len + (len & 1);
    }
    Some(out)
}

/// Drops the EXIF and XMP chunks (and says so in the header).
fn strip_webp(d: &[u8]) -> Option<Vec<u8>> {
    let mut body = b"WEBP".to_vec();
    for (id, data) in riff_chunks(d)? {
        if matches!(&id, b"EXIF" | b"XMP ") {
            continue;
        }
        let mut data = data.to_vec();
        if &id == b"VP8X"
            && let Some(flags) = data.first_mut()
        {
            *flags &= !(0x08 | 0x04);
        }
        body.extend_from_slice(&id);
        body.extend_from_slice(&(data.len() as u32).to_le_bytes());
        let odd = data.len() & 1 == 1;
        body.extend_from_slice(&data);
        if odd {
            body.push(0);
        }
    }
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Some(out)
}

/// Drops comments and application data (XMP lives there) but keeps the
/// looping setting (NETSCAPE/ANIMEXTS) so animations still loop.
fn strip_gif(d: &[u8]) -> Option<Vec<u8>> {
    if d.len() < 13 || !(d.starts_with(b"GIF87a") || d.starts_with(b"GIF89a")) {
        return None;
    }
    let skip_table = |flags: u8| {
        if flags & 0x80 != 0 {
            3 * (2usize << (flags & 0x07))
        } else {
            0
        }
    };
    let mut i = 13 + skip_table(d[10]);
    let mut out = d.get(..i)?.to_vec();
    // Sub-blocks from `i`, ending with an empty one: where they end.
    let blocks_end = |mut i: usize| -> Option<usize> {
        loop {
            let n = *d.get(i)? as usize;
            i += 1 + n;
            if n == 0 {
                return Some(i);
            }
        }
    };
    loop {
        match *d.get(i)? {
            0x3B => {
                out.push(0x3B);
                return Some(out);
            }
            0x2C => {
                let flags = *d.get(i + 9)?;
                let data = i + 10 + skip_table(flags) + 1; // + LZW code size
                let end = blocks_end(data)?;
                out.extend_from_slice(d.get(i..end)?);
                i = end;
            }
            0x21 => {
                let label = *d.get(i + 1)?;
                let end = blocks_end(i + 2)?;
                let keep = match label {
                    0xFE => false,
                    0xFF => d
                        .get(i + 3..i + 14)
                        .is_some_and(|app| app == b"NETSCAPE2.0" || app == b"ANIMEXTS1.0"),
                    _ => true,
                };
                if keep {
                    out.extend_from_slice(d.get(i..end)?);
                }
                i = end;
            }
            _ => return None,
        }
    }
}

/// A video or sound file without its metadata (title, location, recording
/// device and software, dates, chapters, cover pictures), copied by ffmpeg
/// without re-encoding. `None` without ffmpeg, or when it can't.
fn strip_av(data: &[u8], name: &str, video: bool) -> Option<Vec<u8>> {
    use std::process::{Command, Stdio};
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| (1..=5).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric()))?
        .to_ascii_lowercase();
    let dir = std::env::temp_dir().join(format!(
        "whisker-up-{}",
        &crate::vault::random_secret()[..16]
    ));
    crate::stores::make_private_dir(&dir).ok()?;
    let (src, out) = (
        dir.join(format!("in.{ext}")),
        dir.join(format!("out.{ext}")),
    );
    let result = (|| {
        std::fs::write(&src, data).ok()?;
        crate::vault::owner_only(&src, false);
        // Pictures, sound and subtitles only: data tracks (phones keep
        // location and sensor readings there) and attachments are left out.
        let maps: &[&str] = if video {
            &["-map", "0:v", "-map", "0:a?", "-map", "0:s?"]
        } else {
            &["-map", "0:a"]
        };
        let ok = Command::new("ffmpeg")
            // Only the file itself: a playlist can't make ffmpeg fetch anything.
            .args(["-v", "error", "-nostdin", "-y", "-protocol_whitelist", "file", "-i"])
            .arg(&src)
            .args(maps)
            .args([
                "-map_metadata",
                "-1",
                // Each stream's own tags too (a phone's creation time, its name).
                "-map_metadata:s:v",
                "-1",
                "-map_metadata:s:a",
                "-1",
                "-map_metadata:s:s",
                "-1",
                "-map_chapters",
                "-1",
                "-c",
                "copy",
                "-fflags",
                "+bitexact",
                "-flags:v",
                "+bitexact",
                "-flags:a",
                "+bitexact",
            ])
            .arg(&out)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?
            .success();
        ok.then(|| std::fs::read(&out).ok())
            .flatten()
            .filter(|d| !d.is_empty())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// The first 32 hex digits of the SHA-256 of `data`, with an extension for
/// its type.
fn hashed_name(data: &[u8], mime: &Mime, original: &str) -> String {
    use sha2::Digest;
    let hex: String = sha2::Sha256::digest(data)
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect();
    let ext = match mime.essence_str() {
        "image/jpeg" => Some("jpg".to_owned()),
        "image/png" => Some("png".to_owned()),
        "image/webp" => Some("webp".to_owned()),
        "image/gif" => Some("gif".to_owned()),
        _ => std::path::Path::new(original)
            .extension()
            .and_then(|e| e.to_str())
            .filter(|e| (1..=5).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric()))
            .map(str::to_ascii_lowercase),
    };
    match ext {
        Some(e) => format!("{hex}.{e}"),
        None => hex,
    }
}

/// Time allowed for an upload of `n` bytes (slow links included).
fn upload_time(n: usize) -> Duration {
    Duration::from_secs(120 + n as u64 / 8_000)
}

/// Uploads a prepared file (encrypted when the room is, or when
/// `always_encrypt`), calling `progress` with the percentage sent, and
/// returns the message to post.
pub async fn upload(
    room: &Room,
    file: Prepared,
    always_encrypt: bool,
    progress: impl Fn(u8) + Send + 'static,
) -> Result<MessageType> {
    let client = room.client();
    let encrypt = always_encrypt
        || room
            .latest_encryption_state()
            .await
            .map(|s| s.is_encrypted())
            .unwrap_or(true);
    let sent = eyeball::SharedObservable::new(matrix_sdk::TransmissionProgress::default());
    let mut watch = sent.subscribe();
    tokio::spawn(async move {
        let mut last = None;
        while let Some(p) = watch.next().await {
            if p.total == 0 {
                continue;
            }
            let pct = (p.current.min(p.total) as u64 * 100 / p.total as u64) as u8;
            if last != Some(pct) {
                last = Some(pct);
                progress(pct);
            }
        }
    });
    let config = |n: usize| {
        RequestConfig::default()
            .retry_limit(3)
            .timeout(upload_time(n))
    };
    let put =
        async |data: &[u8],
               mime: &Mime,
               observe: Option<eyeball::SharedObservable<matrix_sdk::TransmissionProgress>>|
               -> Result<MediaSource> {
            if encrypt {
                let mut cursor = Cursor::new(data);
                let mut req = client
                    .upload_encrypted_file(&mut cursor)
                    .with_request_config(config(data.len()));
                if let Some(o) = observe {
                    req = req.with_send_progress_observable(o);
                }
                Ok(MediaSource::Encrypted(Box::new(req.await?)))
            } else {
                let mut req = client
                    .media()
                    .upload(mime, data.to_vec(), Some(config(data.len())));
                if let Some(o) = observe {
                    req = req.with_send_progress_observable(o);
                }
                Ok(MediaSource::Plain(req.await?.content_uri))
            }
        };
    let thumb = match &file.thumb {
        Some(t) => Some((
            put(&t.data, &t.mime, None)
                .await
                .context("couldn't upload the preview")?,
            t,
        )),
        None => None,
    };
    let source = put(&file.data, &file.mime, Some(sent)).await?;
    let thumb_info = thumb.map(|(src, t)| {
        let mut info = ThumbnailInfo::new();
        info.width = Some(UInt::from(t.size.0));
        info.height = Some(UInt::from(t.size.1));
        info.size = UInt::try_from(t.data.len() as u64).ok();
        info.mimetype = Some(t.mime.essence_str().to_owned());
        (src, Box::new(info))
    });
    let bytes = UInt::try_from(file.data.len() as u64).ok();
    let mime = Some(file.mime.essence_str().to_owned());
    let (w, h) = (
        file.size.map(|s| UInt::from(s.0)),
        file.size.map(|s| UInt::from(s.1)),
    );
    let (thumb_source, thumb_info) = thumb_info.map_or((None, None), |(s, i)| (Some(s), Some(i)));
    Ok(match file.mime.type_() {
        mime_guess::mime::IMAGE => {
            let mut info = ImageInfo::new();
            (
                info.mimetype,
                info.size,
                info.width,
                info.height,
                info.thumbnail_source,
                info.thumbnail_info,
            ) = (mime, bytes, w, h, thumb_source, thumb_info);
            MessageType::Image(
                ImageMessageEventContent::new(file.name, source).info(Some(Box::new(info))),
            )
        }
        mime_guess::mime::VIDEO => {
            let mut info = VideoInfo::new();
            (
                info.mimetype,
                info.size,
                info.width,
                info.height,
                info.duration,
                info.thumbnail_source,
                info.thumbnail_info,
            ) = (mime, bytes, w, h, file.duration, thumb_source, thumb_info);
            MessageType::Video(
                VideoMessageEventContent::new(file.name, source).info(Some(Box::new(info))),
            )
        }
        mime_guess::mime::AUDIO => {
            let mut info = AudioInfo::new();
            (info.mimetype, info.size) = (mime, bytes);
            MessageType::Audio(
                AudioMessageEventContent::new(file.name, source).info(Some(Box::new(info))),
            )
        }
        _ => {
            let mut info = FileInfo::new();
            (
                info.mimetype,
                info.size,
                info.thumbnail_source,
                info.thumbnail_info,
            ) = (mime, bytes, thumb_source, thumb_info);
            MessageType::File(
                FileMessageEventContent::new(file.name, source).info(Some(Box::new(info))),
            )
        }
    })
}

/// Posts an uploaded file's message.
pub async fn post(room: &Room, message: MessageType) -> Result<OwnedEventId> {
    let config = RequestConfig::default()
        .retry_limit(6)
        .max_retry_time(Duration::from_secs(120));
    Ok(room
        .send(RoomMessageEventContent::new(message))
        .with_request_config(config)
        .await?
        .response
        .event_id)
}

/// Posts an uploaded file's message as a reply to `reply_to` (shown as
/// "replying to …" with the file under it).
pub async fn post_reply(
    room: &Room,
    message: MessageType,
    reply_to: Option<&matrix_sdk::ruma::EventId>,
) -> Result<OwnedEventId> {
    use matrix_sdk::room::reply::{EnforceThread, Reply};
    use matrix_sdk::ruma::events::room::message::{
        AddMentions, RoomMessageEventContentWithoutRelation,
    };
    let Some(id) = reply_to else {
        return post(room, message).await;
    };
    let content = room
        .make_reply_event(
            RoomMessageEventContentWithoutRelation::new(message),
            Reply {
                event_id: id.to_owned(),
                enforce_thread: EnforceThread::MaybeThreaded,
                add_mentions: AddMentions::Yes,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("couldn't build the reply: {e}"))?;
    let config = RequestConfig::default()
        .retry_limit(6)
        .max_retry_time(Duration::from_secs(120));
    Ok(room
        .send(content)
        .with_request_config(config)
        .await?
        .response
        .event_id)
}

/// Puts a caption on an uploaded file's message, the way the spec (and
/// Element) does it: the caption becomes the body (Markdown allowed) and the
/// file's name moves to `filename`.
pub fn with_caption(message: MessageType, caption: &str) -> MessageType {
    use matrix_sdk::ruma::events::room::message::FormattedBody;
    let caption = caption.trim();
    if caption.is_empty() {
        return message;
    }
    let formatted = FormattedBody::markdown(caption);
    macro_rules! caption {
        ($c:expr) => {{
            let mut c = $c;
            let name = std::mem::replace(&mut c.body, caption.to_owned());
            c.filename.get_or_insert(name);
            c.formatted = formatted;
            c
        }};
    }
    match message {
        MessageType::Image(c) => MessageType::Image(caption!(c)),
        MessageType::Video(c) => MessageType::Video(caption!(c)),
        MessageType::Audio(c) => MessageType::Audio(caption!(c)),
        MessageType::File(c) => MessageType::File(caption!(c)),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn files_it_cant_clean_are_held_back() {
        let opts = UploadOptions::default();
        assert!(opts.strip_metadata);
        let pdf = prepare("report.pdf", b"%PDF-1.7 /Author (Me)".to_vec(), &opts, None);
        assert!(pdf.unclean.as_deref().is_some_and(|w| w.contains("PDF")));
        let heic = prepare("IMG_0001.HEIC", vec![0, 0, 0, 24, b'f', b't', b'y', b'p', b'h', b'e', b'i', b'c'], &opts, None);
        assert!(heic.unclean.is_some());
        // Plain text has nothing to remove; with the setting off, nothing is held.
        assert!(prepare("notes.txt", b"hi".to_vec(), &opts, None).unclean.is_none());
        let off = UploadOptions { strip_metadata: false, ..UploadOptions::default() };
        assert!(prepare("report.pdf", b"%PDF".to_vec(), &off, None).unclean.is_none());
        // TIFF is redrawn as PNG.
        let img = image::RgbImage::from_pixel(8, 8, image::Rgb([1, 2, 3]));
        let mut tiff = Cursor::new(Vec::new());
        img.write_to(&mut tiff, ImageFormat::Tiff).unwrap();
        let t = prepare("scan.tiff", tiff.into_inner(), &opts, None);
        assert!(t.unclean.is_none());
        assert_eq!(t.mime.essence_str(), "image/png");
    }

    use super::*;

    /// A tiny JPEG with EXIF (and a comment), made here.
    fn jpeg_with_exif() -> Vec<u8> {
        let img =
            DynamicImage::ImageRgb8(image::RgbImage::from_pixel(4, 4, image::Rgb([200, 10, 10])));
        let plain = encode(&img, ImageFormat::Jpeg, 90).unwrap();
        // Big-endian TIFF with one tag: GPS info offset (0x8825), value 0, plus a marker string.
        let mut exif =
            b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x01\x88\x25\0\x04\0\0\0\x01\0\0\0\0\0\0\0\0SECRET-GPS"
                .to_vec();
        let mut seg = vec![0xFF, 0xE1];
        seg.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        seg.append(&mut exif);
        let mut com = vec![0xFF, 0xFE, 0, 12];
        com.extend_from_slice(b"SECRET-CMT");
        let mut out = plain[..2].to_vec();
        out.extend(seg);
        out.extend(com);
        out.extend_from_slice(&plain[2..]);
        // Something after the end, as phones add.
        out.extend_from_slice(b"\xFF\xD8SECRET-TAIL\xFF\xD9");
        out
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn strips_jpeg_metadata_and_keeps_the_picture() {
        let src = jpeg_with_exif();
        assert!(contains(&src, b"SECRET-GPS"));
        let out = strip(&src, ImageFormat::Jpeg).unwrap();
        for s in [&b"SECRET-GPS"[..], b"SECRET-CMT", b"SECRET-TAIL"] {
            assert!(!contains(&out, s));
        }
        let a = image::load_from_memory(&src).unwrap().to_rgb8();
        let b = image::load_from_memory(&out).unwrap().to_rgb8();
        assert_eq!(a, b, "the pixels didn't change");
    }

    #[test]
    fn strips_png_text() {
        let img = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            3,
            3,
            image::Rgba([1, 2, 3, 255]),
        ));
        let plain = encode(&img, ImageFormat::Png, 0).unwrap();
        let mut chunk = Vec::new();
        let body = b"Author\0SECRET-NAME";
        chunk.extend_from_slice(&(body.len() as u32).to_be_bytes());
        chunk.extend_from_slice(b"tEXt");
        chunk.extend_from_slice(body);
        chunk.extend_from_slice(&[0; 4]);
        let mut src = plain[..33].to_vec(); // signature + IHDR
        src.extend(chunk);
        src.extend_from_slice(&plain[33..]);
        let out = strip(&src, ImageFormat::Png).unwrap();
        assert!(!contains(&out, b"SECRET-NAME"));
        assert_eq!(
            image::load_from_memory(&out).unwrap().to_rgba8(),
            img.to_rgba8()
        );
    }

    #[test]
    fn strips_webp_exif() {
        let vp8l = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2,
            2,
            image::Rgba([9, 9, 9, 255]),
        ));
        let mut plain = Vec::new();
        vp8l.write_to(&mut Cursor::new(&mut plain), ImageFormat::WebP)
            .unwrap();
        let chunks = riff_chunks(&plain).unwrap();
        let mut body = b"WEBP".to_vec();
        let mut vp8x = vec![0x08, 0, 0, 0];
        vp8x.extend_from_slice(&[1, 0, 0, 1, 0, 0]); // 2x2 (minus one)
        for (id, data) in [(*b"VP8X", &vp8x[..])]
            .into_iter()
            .chain(chunks.iter().map(|(i, d)| (*i, *d)))
            .chain([(*b"EXIF", &b"SECRET-EXIF!"[..])])
        {
            body.extend_from_slice(&id);
            body.extend_from_slice(&(data.len() as u32).to_le_bytes());
            body.extend_from_slice(data);
            if data.len() & 1 == 1 {
                body.push(0);
            }
        }
        let mut src = b"RIFF".to_vec();
        src.extend_from_slice(&(body.len() as u32).to_le_bytes());
        src.extend(body);
        let out = strip(&src, ImageFormat::WebP).unwrap();
        assert!(!contains(&out, b"SECRET-EXIF"));
        assert_eq!(riff_chunks(&out).unwrap()[0].1[0] & 0x08, 0);
        assert_eq!(
            image::load_from_memory(&out).unwrap().to_rgba8(),
            vp8l.to_rgba8()
        );
    }

    #[test]
    fn strips_gif_comments() {
        let mut src = Vec::new();
        DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2,
            2,
            image::Rgba([5, 5, 5, 255]),
        ))
        .write_to(&mut Cursor::new(&mut src), ImageFormat::Gif)
        .unwrap();
        let at = src.len() - 1;
        src.splice(at..at, b"\x21\xFE\x0ASECRET-GIF\x00".iter().copied());
        assert!(image::load_from_memory(&src).is_ok());
        let out = strip(&src, ImageFormat::Gif).unwrap();
        assert!(!contains(&out, b"SECRET-GIF"));
        assert!(image::load_from_memory(&out).is_ok());
    }

    #[test]
    fn names_by_hash_and_makes_previews() {
        let big = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            1600,
            1200,
            image::Rgb([0, 128, 255]),
        ));
        let png = encode(&big, ImageFormat::Png, 0).unwrap();
        let p = prepare("holiday at home.png", png, &UploadOptions::default(), None);
        assert!(!p.name.contains("holiday"));
        assert!(p.name.ends_with(".png") && p.name.len() == 36, "{}", p.name);
        assert_eq!(p.size, Some((1600, 1200)));
        let t = p.thumb.unwrap();
        assert_eq!((t.size, t.mime.essence_str()), ((800, 600), "image/jpeg"));
    }

    #[test]
    fn converts_but_keeps_transparent_pictures() {
        let opts = UploadOptions {
            convert: Convert::Jpeg,
            ..Default::default()
        };
        let solid = encode(
            &DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                4,
                4,
                image::Rgba([1, 2, 3, 255]),
            )),
            ImageFormat::Png,
            0,
        )
        .unwrap();
        assert_eq!(
            prepare("a.png", solid, &opts, None).mime.essence_str(),
            "image/jpeg"
        );
        let clear = encode(
            &DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                4,
                4,
                image::Rgba([1, 2, 3, 0]),
            )),
            ImageFormat::Png,
            0,
        )
        .unwrap();
        assert_eq!(
            prepare("b.png", clear, &opts, None).mime.essence_str(),
            "image/png"
        );
    }

    #[test]
    fn leaves_other_files_alone_but_renames_them() {
        let p = prepare(
            "Secret Plans.PDF",
            b"%PDF-1.4 hello".to_vec(),
            &UploadOptions::default(),
            None,
        );
        assert_eq!(p.data, b"%PDF-1.4 hello");
        assert!(p.name.ends_with(".pdf") && !p.name.contains("Secret"));
        let kept = prepare(
            "Secret Plans.pdf",
            b"x".to_vec(),
            &UploadOptions {
                hash_names: false,
                ..Default::default()
            },
            None,
        );
        assert_eq!(kept.name, "Secret Plans.pdf");
    }

    /// "Always encrypt" in a room without encryption: what reaches the
    /// server is scrambled, and the message carries the key.
    #[tokio::test]
    async fn always_encrypt_scrambles_the_upload() {
        use wiremock::matchers::{method, path_regex};
        let mock = matrix_sdk::test_utils::mocks::MatrixMockServer::new().await;
        let client = mock.client_builder().build().await;
        let room = mock
            .sync_joined_room(&client, matrix_sdk::ruma::room_id!("!r:localhost"))
            .await;
        mock.mock_room_state_encryption().plain().mount().await;
        wiremock::Mock::given(path_regex("/media/config$"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"m.upload.size": 50_000_000})),
            )
            .mount(mock.server())
            .await;
        wiremock::Mock::given(method("POST"))
            .and(path_regex("/upload$"))
            .and(|r: &wiremock::Request| !contains(&r.body, b"PLAINTEXT-MARKER"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"content_uri": "mxc://localhost/x"})),
            )
            .expect(1)
            .mount(mock.server())
            .await;
        let file = prepare(
            "notes.txt",
            b"PLAINTEXT-MARKER secret notes".to_vec(),
            &UploadOptions::default(),
            None,
        );
        let message = upload(&room, file, true, |_| ()).await.unwrap();
        let MessageType::File(f) = message else {
            panic!("not a file message")
        };
        assert!(matches!(f.source, MediaSource::Encrypted(_)));
    }

    /// A phone-like video's title, location and dates are gone; the video
    /// still plays.
    #[test]
    fn strips_video_metadata() {
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("whisker-vid-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("clip.mp4");
        let made = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=10:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=1",
                "-shortest",
                "-pix_fmt",
                "yuv420p",
            ])
            .args([
                "-metadata",
                "title=SECRET-TITLE",
                "-metadata",
                "location=+48.8582+002.2945/",
                "-metadata",
                "creation_time=2024-05-01T10:00:00Z",
                "-metadata:s:v",
                "handler_name=SECRET-PHONE",
                "-movflags",
                "use_metadata_tags",
            ])
            .arg(&video)
            .status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("no ffmpeg; skipping");
            return;
        }
        let src = std::fs::read(&video).unwrap();
        assert!(contains(&src, b"SECRET-TITLE") && contains(&src, b"+48.8582"));
        let p = prepare("holiday.mp4", src, &UploadOptions::default(), None);
        for s in [&b"SECRET-TITLE"[..], b"+48.8582", b"SECRET-PHONE", b"Lavf"] {
            assert!(
                !contains(&p.data, s),
                "{} left in",
                String::from_utf8_lossy(s)
            );
        }
        let clean = dir.join("clean.mp4");
        std::fs::write(&clean, &p.data).unwrap();
        let info = crate::timeline::probe_video(&clean).expect("still a video");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!((info.width, info.height, info.audio), (320, 240, true));
        assert!(p.name.ends_with(".mp4") && !p.name.contains("holiday"));
    }

    #[test]
    fn captions_keep_the_file_name() {
        let src = matrix_sdk::ruma::OwnedMxcUri::from("mxc://x.org/a");
        let pic = MessageType::Image(ImageMessageEventContent::plain("cat.png".into(), src));
        let MessageType::Image(c) = with_caption(pic.clone(), "my **cat**") else {
            panic!()
        };
        assert_eq!(
            (c.body.as_str(), c.filename.as_deref()),
            ("my **cat**", Some("cat.png"))
        );
        assert!(
            c.formatted
                .is_some_and(|f| f.body.contains("<strong>cat</strong>"))
        );
        assert!(
            matches!(with_caption(pic, "  "), MessageType::Image(c) if c.body == "cat.png" && c.filename.is_none())
        );
    }
}
