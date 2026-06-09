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
//! via achromat, known text/code types, and a sniffing fallback that
//! distinguishes unknown text from binary with a single bounded read.

mod image;
mod text;

use std::path::Path;

pub use image::ImageHandler;
pub use text::{TextFallbackHandler, TextHandler};

use achromat::convert::ImageMeta;
use damascene_core::image::Image;

/// A loaded preview, ready for the UI to render.
pub enum Preview {
    /// Color-managed raster image (full size; the UI letterboxes it).
    Image { image: Image, meta: ImageMeta },
    /// Text prefix of the file. `truncated` when the file goes on
    /// beyond what was read.
    Text { text: String, truncated: bool },
    /// Recognized but deliberately not previewed (binary data, …).
    /// Distinct from `load` returning `Err`, which means a preview was
    /// attempted and failed.
    Unsupported { reason: String },
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
    /// an unknown extension is called binary, and an unknown extension
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

        let bin_path = dir.join("blob.dat");
        let mut f = std::fs::File::create(&bin_path).unwrap();
        f.write_all(&[0u8, 159, 146, 150, 0, 1, 2, 3]).unwrap();
        drop(f);
        match Registry::standard().load(&bin_path).unwrap() {
            Preview::Unsupported { reason } => assert!(reason.contains("binary"), "{reason}"),
            _ => panic!("NUL-bearing data should be unsupported"),
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
