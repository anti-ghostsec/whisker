//! Typst: maths and small typeset documents, drawn as pictures here.
//!
//! Off unless turned on in Settings. What's typeset sees only its own text:
//! no files, no packages (so nothing is fetched), not even today's date (a
//! document could put it in the picture it's sent as). Typst's own fonts are
//! built in, so a picture looks the same on every device and says nothing
//! about which fonts this one has.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use typst::diag::{FileError, FileResult, Severity};
use typst::foundations::{Bytes, Datetime};
use typst::syntax::{FileId, Source};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};
use typst_layout::PagedDocument;

/// Longest text typeset (bytes).
pub const MAX_SOURCE: usize = 8 * 1024;
/// Pixels per point: sharp on high-density screens.
const SCALE: f64 = 3.0;
/// Widest and tallest picture made.
const MAX_PX: f64 = 2400.0;
/// Time allowed before giving up on one.
const TIME: Duration = Duration::from_secs(10);

/// One at a time: a document that takes long can't be stopped, only left
/// to finish, and it shouldn't be joined by more.
static BUSY: AtomicBool = AtomicBool::new(false);

struct Fonts {
    book: LazyHash<FontBook>,
    fonts: Vec<Font>,
}

static FONTS: LazyLock<Fonts> = LazyLock::new(|| {
    let fonts: Vec<Font> = typst_assets::fonts().flat_map(|data| Font::iter(Bytes::new(data))).collect();
    Fonts { book: LazyHash::new(FontBook::from_fonts(&fonts)), fonts }
});

static LIBRARY: LazyLock<LazyHash<Library>> = LazyLock::new(|| LazyHash::new(<Library as LibraryExt>::default()));

/// A world of one file: the text being typeset.
struct One {
    main: Source,
}

impl World for One {
    fn library(&self) -> &LazyHash<Library> {
        &LIBRARY
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &FONTS.book
    }

    fn main(&self) -> FileId {
        self.main.id()
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.main.id() { Ok(self.main.clone()) } else { Err(FileError::AccessDenied) }
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        if id == self.main.id() { Ok(Bytes::from_string(self.main.text().to_owned())) } else { Err(FileError::AccessDenied) }
    }

    fn font(&self, index: usize) -> Option<Font> {
        FONTS.fonts.get(index).cloned()
    }

    fn today(&self, _offset: Option<typst::foundations::Duration>) -> Option<Datetime> {
        None
    }
}

/// `src` on a page that fits it: white, a small margin, readable text.
fn page(src: &str) -> String {
    format!("#set page(width: auto, height: auto, margin: 10pt, fill: white)\n#set text(size: 14pt)\n{src}")
}

/// `src` typeset (its first page), or why it couldn't be.
pub fn render(src: &str) -> Result<image::RgbaImage, String> {
    if src.len() > MAX_SOURCE {
        return Err(format!("that's too long to typeset here (at most {} KB)", MAX_SOURCE / 1024));
    }
    if src.trim().is_empty() {
        return Err("there's nothing to typeset".into());
    }
    let world = One { main: Source::detached(page(src)) };
    let doc = typst::compile::<PagedDocument>(&world).output.map_err(|errors| {
        let first = errors.iter().find(|e| e.severity == Severity::Error).map(|e| e.message.to_string());
        format!("Typst: {}", first.unwrap_or_else(|| "it couldn't be typeset".into()))
    })?;
    let page = doc.pages().first().ok_or("Typst: there's nothing to show")?;
    let size = page.frame.size();
    let (w, h) = (size.x.to_pt(), size.y.to_pt());
    if !(w > 0.0 && h > 0.0) {
        return Err("Typst: there's nothing to show".into());
    }
    let scale = SCALE.min(MAX_PX / w).min(MAX_PX / h);
    let pixmap = typst_render::render(page, &typst_render::RenderOptions { pixel_per_pt: scale.into(), render_bleed: false });
    let (pw, ph) = (pixmap.width(), pixmap.height());
    image::RgbaImage::from_raw(pw, ph, pixmap.take_demultiplied()).ok_or_else(|| "Typst: the picture couldn't be made".into())
}

/// [`render`] off the async threads, one at a time, with a time limit.
pub async fn render_async(src: String) -> Result<image::RgbaImage, String> {
    if BUSY.swap(true, Ordering::SeqCst) {
        return Err("still typesetting the last one; try again in a moment".into());
    }
    let job = tokio::task::spawn_blocking(move || {
        let r = render(&src);
        BUSY.store(false, Ordering::SeqCst);
        r
    });
    match tokio::time::timeout(TIME, job).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => {
            BUSY.store(false, Ordering::SeqCst);
            Err("Typst stopped unexpectedly".into())
        }
        // It carries on in the background (it can't be stopped), and
        // nothing else is typeset until it's done.
        Err(_) => Err("that took too long to typeset".into()),
    }
}

/// `image` as a PNG file.
pub fn png(image: &image::RgbaImage) -> Option<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// The Typst in a message: the text of its ```typst code blocks, joined.
/// `None` when it has none.
pub fn blocks(body: &str) -> Option<String> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("```typst") {
        let after = &rest[start + "```typst".len()..];
        // The fence's own line ends first.
        let Some(nl) = after.find('\n') else { break };
        if !after[..nl].trim().is_empty() {
            rest = &after[nl..];
            continue;
        }
        let code = &after[nl + 1..];
        let Some(end) = code.find("```") else { break };
        let text = code[..end].trim_end();
        if !text.trim().is_empty() {
            out.push(text.to_owned());
        }
        rest = &code[end + 3..];
    }
    (!out.is_empty()).then(|| out.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typesets_maths() {
        let img = render("$ sum_(k=1)^n k = (n(n+1))/2 $").unwrap();
        assert!(img.width() > 50 && img.height() > 20);
        // A white page with something dark on it.
        assert!(img.pixels().any(|p| p.0[0] < 100));
        assert_eq!(img.get_pixel(0, 0).0, [255, 255, 255, 255]);
    }

    #[test]
    fn reaches_nothing_outside() {
        // No files, no packages, no date.
        assert!(render("#read(\"/etc/hostname\")").is_err());
        assert!(render("#import \"@preview/cetz:0.3.0\"").is_err());
        assert!(render("#datetime.today().display()").is_err());
        assert!(render("#image(\"/etc/passwd\")").is_err());
    }

    #[test]
    fn errors_and_limits() {
        let e = render("#let x = ").unwrap_err();
        assert!(e.starts_with("Typst:"), "{e}");
        assert!(render("   ").is_err());
        assert!(render(&"a".repeat(MAX_SOURCE + 1)).is_err());
        // A huge page is drawn smaller, not huge.
        let big = render("#box(width: 5000pt, height: 10pt)").unwrap();
        assert!(big.width() <= MAX_PX as u32 + 1);
        assert!(png(&big).is_some_and(|b| b.starts_with(b"\x89PNG")));
    }

    #[test]
    fn finds_typst_blocks() {
        assert_eq!(blocks("hi\n```typst\n$x^2$\n```\nbye").as_deref(), Some("$x^2$"));
        assert_eq!(blocks("```typst\na\n```\n```typst\nb\n```").as_deref(), Some("a\n\nb"));
        assert!(blocks("```rust\nfn x(){}\n```").is_none());
        assert!(blocks("```typst\n\n```").is_none());
        assert!(blocks("```typstx\na\n```").is_none());
        assert!(blocks("no code").is_none());
    }
}
