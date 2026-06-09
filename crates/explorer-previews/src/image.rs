//! Color-managed image previews via achromat.

use std::path::Path;

use achromat::{convert, decode};

use crate::{extension, Preview, PreviewHandler};

/// Extensions the achromat pipeline decodes. Dispatch inside `load` is
/// by magic bytes — this list only routes; a mislabeled file still
/// decodes as what it really is.
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "jxr", "jxl", "avif", "png", "jpg", "jpeg", "webp", "exr", "hdr",
];

pub struct ImageHandler;

impl PreviewHandler for ImageHandler {
    fn name(&self) -> &'static str {
        "image"
    }

    fn claims(&self, path: &Path) -> bool {
        extension(path).is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.as_str()))
    }

    fn load(&self, path: &Path) -> anyhow::Result<Preview> {
        // Straight alpha: damascene premultiplies at blend time.
        let decoded = decode::load_straight(path)?;
        Ok(Preview::Image {
            meta: convert::meta_of(&decoded),
            image: convert::to_damascene(&decoded),
        })
    }
}
