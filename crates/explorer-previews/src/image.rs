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
        load_raster_image(path)
    }
}

pub(crate) fn load_raster_image(path: &Path) -> anyhow::Result<Preview> {
    // Straight alpha: damascene premultiplies at blend time.
    let decoded = decode::load_straight(path)?;
    let meta = convert::meta_of(&decoded);
    // Cap the uploaded texture: GPU texture limits are commonly
    // 8192 (wgpu's default request), and a preview pane never
    // needs a 12800px source. The linear-light thumbnailer keeps
    // HDR highlights; `meta` keeps reporting the true dimensions.
    let image = if decoded.width.max(decoded.height) > MAX_PREVIEW_EDGE {
        convert::thumbnail(&decoded, MAX_PREVIEW_EDGE)
    } else {
        convert::to_damascene(&decoded)
    };
    Ok(Preview::Image { image, meta })
}

/// Long-edge cap for preview uploads, comfortably under every desktop
/// adapter's texture dimension limit (the floor in practice is 4096;
/// wgpu's default request is 8192).
const MAX_PREVIEW_EDGE: u32 = 4096;
