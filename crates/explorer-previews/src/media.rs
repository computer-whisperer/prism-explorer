//! Media metadata previews.

use std::io::Read;
use std::path::Path;

use crate::{extension, DetailRow, Preview, PreviewHandler};

const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mov", "webm", "mkv", "avi", "mpeg", "mpg", "m2ts", "mts", "ts", "ogv",
];
const HEAD_BYTES: usize = 64 * 1024;

pub struct VideoHandler;

impl PreviewHandler for VideoHandler {
    fn name(&self) -> &'static str {
        "video"
    }

    fn claims(&self, path: &Path) -> bool {
        extension(path).is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.as_str()))
    }

    fn load(&self, path: &Path) -> anyhow::Result<Preview> {
        let head = read_head(path)?;
        let ext = extension(path);
        let info = video_info(&head, ext.as_deref());
        let mut rows = vec![DetailRow {
            label: "Container".into(),
            value: info.container.into(),
        }];
        if let Some(brand) = info.brand {
            rows.push(DetailRow {
                label: "Brand".into(),
                value: brand,
            });
        }
        rows.push(DetailRow {
            label: "Preview".into(),
            value: "metadata only".into(),
        });

        Ok(Preview::Details {
            icon: "activity",
            title: "Video file".into(),
            rows,
        })
    }
}

struct VideoInfo {
    container: &'static str,
    brand: Option<String>,
}

fn read_head(path: &Path) -> anyhow::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    file.by_ref()
        .take(HEAD_BYTES as u64)
        .read_to_end(&mut buf)?;
    Ok(buf)
}

fn video_info(head: &[u8], ext: Option<&str>) -> VideoInfo {
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        let major = ascii_fourcc(&head[8..12]).unwrap_or_else(|| "unknown".into());
        return VideoInfo {
            container: match major.as_str() {
                "qt  " => "QuickTime",
                "M4V " | "M4A " => "MPEG-4",
                _ => "ISO BMFF / MP4",
            },
            brand: Some(major.trim().to_string()),
        };
    }
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"AVI " {
        return VideoInfo {
            container: "AVI",
            brand: None,
        };
    }
    if head.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return VideoInfo {
            container: if find_bytes(head, b"webm").is_some() {
                "WebM"
            } else {
                "Matroska"
            },
            brand: None,
        };
    }
    if head.starts_with(b"OggS") {
        return VideoInfo {
            container: "Ogg",
            brand: None,
        };
    }
    if head.starts_with(&[0x00, 0x00, 0x01, 0xba]) {
        return VideoInfo {
            container: "MPEG program stream",
            brand: None,
        };
    }
    if ext.is_some_and(|e| matches!(e, "ts" | "mts" | "m2ts")) {
        return VideoInfo {
            container: "MPEG transport stream",
            brand: None,
        };
    }
    VideoInfo {
        container: "video",
        brand: ext.map(|e| e.to_uppercase()),
    }
}

fn ascii_fourcc(bytes: &[u8]) -> Option<String> {
    (bytes.len() == 4 && bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' '))
        .then(|| String::from_utf8_lossy(bytes).into_owned())
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_common_video_containers() {
        let mp4 = b"\0\0\0\x18ftypisom\0\0\0\0";
        let info = video_info(mp4, Some("mp4"));
        assert_eq!(info.container, "ISO BMFF / MP4");
        assert_eq!(info.brand.as_deref(), Some("isom"));

        let avi = b"RIFF\0\0\0\0AVI ";
        assert_eq!(video_info(avi, Some("avi")).container, "AVI");

        let webm = b"\x1a\x45\xdf\xa3xxxxwebm";
        assert_eq!(video_info(webm, Some("webm")).container, "WebM");
    }
}
