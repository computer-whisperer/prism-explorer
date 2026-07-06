//! Color-managed image previews via achromat.

use std::path::Path;

use achromat::{convert, decode};

use crate::{extension, DetailRow, Preview, PreviewHandler};

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
    // Decode reads the whole file and converts through a full-res f32
    // buffer (16 B/px), so a multi-GB source means a multi-GB read over
    // a possibly-network filesystem and several times that in RAM —
    // enough to OOM the (single-instance) process. Until achromat grows
    // real dimension/allocation limits (achromat#1), refuse by file
    // size; real HDR sources run tens of MB.
    let size = std::fs::metadata(path)?.len();
    if size > MAX_DECODE_BYTES {
        return Ok(Preview::Details {
            icon: "file-text",
            title: "Image file".into(),
            rows: vec![
                DetailRow {
                    label: "Size".into(),
                    value: format!("{:.1} GB", size as f64 / f64::from(1 << 30)),
                },
                DetailRow {
                    label: "Preview".into(),
                    value: "too large to decode".into(),
                },
            ],
        });
    }

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

/// Source-size ceiling for a user-requested decode. GB-scale sources
/// get a details card instead. A size gate can't stop a dimension bomb
/// (a tiny file claiming 50000×50000) — that needs limits inside the
/// decoder, tracked as achromat#1.
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    /// A source past the size ceiling gets a details card without the
    /// file ever being read (sparse: actually reading it would allocate
    /// hundreds of MB and fail decode anyway).
    #[test]
    fn oversized_source_returns_details_not_decode() {
        let dir = std::env::temp_dir().join(format!(
            "explorer-previews-imggate-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("huge.exr");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_DECODE_BYTES + 1).unwrap();

        match load_raster_image(&path).unwrap() {
            Preview::Details { rows, .. } => {
                assert!(rows.iter().any(|r| r.value.contains("too large")), "{rows:?}");
            }
            _ => panic!("oversized image must not be decoded"),
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
