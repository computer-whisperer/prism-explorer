//! Preview handlers: turn a path into something the explorer can show.
//!
//! A [`PreviewHandler`] makes two promises:
//!
//! - [`claims`](PreviewHandler::claims) runs on the **UI thread** and
//!   must do **no IO** — it decides from the file name alone (the
//!   primary target is a slow CephFS mount; even one opportunistic
//!   `open` per row would stall frames). Magic-byte sniffing happens
//!   inside `load`, where the file is being opened anyway.
//! - [`load`](PreviewHandler::load) runs on a worker thread and may
//!   block as long as it likes.
//!
//! [`Registry::standard`] wires the built-ins: color-managed images
//! via achromat, PDF/video poster previews with metadata fallback,
//! WAV metadata, known text/code types, and a sniffing fallback that
//! distinguishes unknown text from visualized binary data with a
//! single bounded read.

mod binary;
mod document;
mod image;
mod media;
mod text;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub use binary::BinaryPreview;
pub use document::PdfHandler;
pub use image::ImageHandler;
pub use media::{AudioHandler, VideoHandler};
pub use text::{raw_preview, RawPreview, TextFallbackHandler, TextHandler};

use achromat::convert::ImageMeta;
use damascene_core::image::Image;

/// A loaded preview, ready for the UI to render.
pub enum Preview {
    /// Color-managed raster image (full size; the UI letterboxes it).
    Image { image: Image, meta: ImageMeta },
    /// Text prefix of the file. `truncated` when the file goes on
    /// beyond what was read.
    Text { text: String, truncated: bool },
    /// Byte-level visualization data for unknown binary files.
    Binary(BinaryPreview),
    /// Structured metadata for recognized formats that are not yet
    /// rendered inline.
    Details {
        icon: &'static str,
        title: String,
        rows: Vec<DetailRow>,
    },
    /// Recognized but deliberately not previewed.
    /// Distinct from `load` returning `Err`, which means a preview was
    /// attempted and failed.
    Unsupported { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailRow {
    pub label: String,
    pub value: String,
}

pub trait PreviewHandler: Send + Sync {
    /// Short name for logs and debugging.
    fn name(&self) -> &'static str;

    /// Whether this handler wants `path`. Called on the UI thread:
    /// file-name inspection only, **no IO**.
    fn claims(&self, path: &Path) -> bool;

    /// Load the preview. Worker thread; may block.
    fn load(&self, path: &Path) -> anyhow::Result<Preview>;
}

/// Ordered handler list; the first handler that claims a path loads it.
pub struct Registry {
    handlers: Vec<Box<dyn PreviewHandler>>,
}

impl Registry {
    /// The built-in chain: images → known text types → sniffing
    /// fallback (which claims everything, so `load` always has a
    /// handler to run).
    pub fn standard() -> Self {
        Registry {
            handlers: vec![
                Box::new(ImageHandler),
                Box::new(PdfHandler),
                Box::new(VideoHandler),
                Box::new(AudioHandler),
                Box::new(TextHandler),
                Box::new(TextFallbackHandler),
            ],
        }
    }

    /// Resolve and run the handler for `path`. Worker thread.
    pub fn load(&self, path: &Path) -> anyhow::Result<Preview> {
        for handler in &self.handlers {
            if handler.claims(path) {
                tracing::debug!(handler = handler.name(), path = %path.display(), "loading preview");
                return handler.load(path);
            }
        }
        Ok(Preview::Unsupported {
            reason: "no preview handler for this file type".into(),
        })
    }
}

/// Case-insensitive extension of `path`, if any.
fn extension(path: &Path) -> Option<String> {
    path.extension().map(|e| e.to_string_lossy().to_lowercase())
}

pub(crate) struct PreviewTempDir {
    path: PathBuf,
}

impl PreviewTempDir {
    pub(crate) fn new(kind: &str) -> anyhow::Result<Self> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);

        let path = std::env::temp_dir().join(format!(
            "prism-explorer-preview-{kind}-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PreviewTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewKind {
    Directory,
    Image,
    Pdf,
    Video,
    Audio,
    Text,
    Other,
}

impl PreviewKind {
    pub fn label(self) -> &'static str {
        match self {
            PreviewKind::Directory => "directory",
            PreviewKind::Image => "image",
            PreviewKind::Pdf => "PDF document",
            PreviewKind::Video => "video",
            PreviewKind::Audio => "audio",
            PreviewKind::Text => "text",
            PreviewKind::Other => "file",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            PreviewKind::Directory => "folder",
            PreviewKind::Image => "file-text",
            PreviewKind::Pdf => "file-text",
            PreviewKind::Video => "activity",
            PreviewKind::Audio => "activity",
            PreviewKind::Text => "file-text",
            PreviewKind::Other => "file",
        }
    }
}

pub fn preview_kind_for_path(path: &Path, is_dir: bool) -> PreviewKind {
    if is_dir {
        return PreviewKind::Directory;
    }
    if ImageHandler.claims(path) {
        PreviewKind::Image
    } else if PdfHandler.claims(path) {
        PreviewKind::Pdf
    } else if VideoHandler.claims(path) {
        PreviewKind::Video
    } else if AudioHandler.claims(path) {
        PreviewKind::Audio
    } else if TextHandler.claims(path) {
        PreviewKind::Text
    } else {
        PreviewKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::path::PathBuf;

    fn scratch_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("explorer-previews-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// End-to-end through the registry: a real PNG decodes to an image
    /// preview, Rust source comes back as text, NUL-bearing data with
    /// an unknown extension gets a binary map, and an unknown extension
    /// with text content falls through to a text preview.
    #[test]
    fn registry_routes_by_type() {
        let dir = scratch_dir();

        let png_path = dir.join("pixel.png");
        {
            let file = std::fs::File::create(&png_path).unwrap();
            let mut enc = png::Encoder::new(file, 2, 2);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&[255u8; 16]).unwrap();
        }
        match Registry::standard().load(&png_path).unwrap() {
            Preview::Image { image, meta } => {
                assert_eq!((image.width(), image.height()), (2, 2));
                assert_eq!((meta.width, meta.height), (2, 2));
            }
            _ => panic!("png should preview as image"),
        }

        let rs_path = dir.join("snippet.rs");
        std::fs::write(&rs_path, "fn main() {}\n").unwrap();
        match Registry::standard().load(&rs_path).unwrap() {
            Preview::Text { text, truncated } => {
                assert!(text.contains("fn main"));
                assert!(!truncated);
            }
            _ => panic!("rust source should preview as text"),
        }

        let pdf_path = dir.join("doc.pdf");
        std::fs::write(
            &pdf_path,
            b"%PDF-1.7\n1 0 obj << /Type /Pages /Count 1 >> endobj\n2 0 obj << /Type /Page /Parent 1 0 R >> endobj\n",
        )
        .unwrap();
        match Registry::standard().load(&pdf_path).unwrap() {
            Preview::Details { title, rows, .. } => {
                assert_eq!(title, "PDF document");
                assert!(rows.iter().any(|r| r.value == "PDF 1.7"));
                assert!(rows.iter().any(|r| r.label == "Pages" && r.value == "1"));
            }
            _ => panic!("pdf should preview as details"),
        }

        let mp4_path = dir.join("clip.mp4");
        std::fs::write(&mp4_path, b"\0\0\0\x18ftypisom\0\0\0\0").unwrap();
        match Registry::standard().load(&mp4_path).unwrap() {
            Preview::Details { title, rows, .. } => {
                assert_eq!(title, "Video file");
                assert!(rows.iter().any(|r| r.value == "ISO BMFF / MP4"));
            }
            _ => panic!("video should preview as details"),
        }

        let wav_path = dir.join("tone.wav");
        std::fs::write(
            &wav_path,
            b"RIFF(\0\0\0WAVEfmt \x10\0\0\0\x01\0\x02\0D\xac\0\0\x10\xb1\x02\0\x04\0\x10\0data\x10\xb1\x02\0",
        )
        .unwrap();
        match Registry::standard().load(&wav_path).unwrap() {
            Preview::Details { title, rows, .. } => {
                assert_eq!(title, "Audio file");
                assert!(rows.iter().any(|r| r.value == "WAV / PCM"));
                assert!(rows.iter().any(|r| r.value == "stereo"));
            }
            _ => panic!("wav should preview as details"),
        }

        let bin_path = dir.join("blob.dat");
        let mut f = std::fs::File::create(&bin_path).unwrap();
        f.write_all(&[0u8, 159, 146, 150, 0, 1, 2, 3]).unwrap();
        drop(f);
        match Registry::standard().load(&bin_path).unwrap() {
            Preview::Binary(preview) => {
                assert_eq!(preview.bytes.len(), 8);
                assert!(preview.nul_fraction > 0.0);
            }
            _ => panic!("NUL-bearing data should preview as binary"),
        }

        // Oversized images downscale below GPU texture limits (the
        // panic class: Device::create_texture beyond 8192); metadata
        // keeps the true dimensions.
        let wide_path = dir.join("wide.png");
        {
            let file = std::fs::File::create(&wide_path).unwrap();
            let mut enc = png::Encoder::new(file, 5000, 8);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&vec![128u8; 5000 * 8 * 4]).unwrap();
        }
        match Registry::standard().load(&wide_path).unwrap() {
            Preview::Image { image, meta } => {
                assert!(
                    image.width().max(image.height()) <= 4096,
                    "{}",
                    image.width()
                );
                assert_eq!(meta.width, 5000);
            }
            _ => panic!("wide png should preview as image"),
        }

        let noext_path = dir.join("NOTES");
        std::fs::write(&noext_path, "remember the milk\n").unwrap();
        match Registry::standard().load(&noext_path).unwrap() {
            Preview::Text { text, .. } => assert!(text.contains("milk")),
            _ => panic!("extensionless text should sniff as text"),
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
