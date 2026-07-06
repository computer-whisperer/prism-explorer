//! Media previews.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context};

use crate::{
    extension, image::load_raster_image, DetailRow, Preview, PreviewHandler, PreviewTempDir,
};

const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mov", "webm", "mkv", "avi", "mpeg", "mpg", "m2ts", "mts", "ts", "ogv",
];
const AUDIO_EXTENSIONS: &[&str] = &["wav", "wave"];
const HEAD_BYTES: usize = 64 * 1024;

pub struct VideoHandler;
pub struct AudioHandler;

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
        if let Ok(preview) = extract_video_frame(path) {
            return Ok(preview);
        }

        Ok(video_details(info, "metadata only"))
    }
}

impl PreviewHandler for AudioHandler {
    fn name(&self) -> &'static str {
        "audio"
    }

    fn claims(&self, path: &Path) -> bool {
        extension(path).is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.as_str()))
    }

    fn load(&self, path: &Path) -> anyhow::Result<Preview> {
        let head = read_head(path)?;
        let Some(info) = wav_info(&head) else {
            return Ok(Preview::Unsupported {
                reason: "not a WAV audio file".into(),
            });
        };
        Ok(audio_details(info))
    }
}

fn extract_video_frame(path: &Path) -> anyhow::Result<Preview> {
    let dir = PreviewTempDir::new("video")?;
    let png = dir.path().join("frame.png");
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-i")
        .arg(path)
        .arg("-frames:v")
        .arg("1")
        .arg("-vf")
        .arg("scale=min(1024\\,iw):-2")
        .arg("-an")
        .arg("-sn")
        .arg(&png)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let status = crate::run_with_timeout(&mut cmd, crate::CHILD_TIMEOUT)
        .with_context(|| "failed to run ffmpeg")?;
    if !status.success() {
        bail!("ffmpeg exited with {status}");
    }

    load_raster_image(&png)
}

fn video_details(info: VideoInfo, preview: &str) -> Preview {
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
        value: preview.into(),
    });

    Preview::Details {
        icon: "activity",
        title: "Video file".into(),
        rows,
    }
}

struct VideoInfo {
    container: &'static str,
    brand: Option<String>,
}

struct WavInfo {
    audio_format: Option<u16>,
    channels: Option<u16>,
    sample_rate: Option<u32>,
    bits_per_sample: Option<u16>,
    data_bytes: Option<u32>,
    byte_rate: Option<u32>,
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

fn wav_info(head: &[u8]) -> Option<WavInfo> {
    if head.len() < 12 || &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        return None;
    }

    let mut info = WavInfo {
        audio_format: None,
        channels: None,
        sample_rate: None,
        bits_per_sample: None,
        data_bytes: None,
        byte_rate: None,
    };
    let mut pos = 12;
    while pos + 8 <= head.len() {
        let id = &head[pos..pos + 4];
        let size = u32::from_le_bytes(head[pos + 4..pos + 8].try_into().ok()?) as usize;
        let data_start = pos + 8;
        let data_end = data_start.saturating_add(size).min(head.len());

        match id {
            b"fmt " if data_end >= data_start + 16 => {
                info.audio_format = Some(u16::from_le_bytes(
                    head[data_start..data_start + 2].try_into().ok()?,
                ));
                info.channels = Some(u16::from_le_bytes(
                    head[data_start + 2..data_start + 4].try_into().ok()?,
                ));
                info.sample_rate = Some(u32::from_le_bytes(
                    head[data_start + 4..data_start + 8].try_into().ok()?,
                ));
                info.byte_rate = Some(u32::from_le_bytes(
                    head[data_start + 8..data_start + 12].try_into().ok()?,
                ));
                info.bits_per_sample = Some(u16::from_le_bytes(
                    head[data_start + 14..data_start + 16].try_into().ok()?,
                ));
            }
            b"data" => {
                info.data_bytes = Some(size as u32);
            }
            _ => {}
        }

        let padded_size = size + (size % 2);
        pos = data_start.saturating_add(padded_size);
        if pos <= data_start {
            break;
        }
    }

    Some(info)
}

fn audio_details(info: WavInfo) -> Preview {
    let mut rows = vec![DetailRow {
        label: "Format".into(),
        value: info
            .audio_format
            .map(wav_format_label)
            .unwrap_or("WAV audio")
            .into(),
    }];
    if let Some(channels) = info.channels {
        rows.push(DetailRow {
            label: "Channels".into(),
            value: channel_label(channels),
        });
    }
    if let Some(sample_rate) = info.sample_rate {
        rows.push(DetailRow {
            label: "Sample rate".into(),
            value: sample_rate_label(sample_rate),
        });
    }
    if let Some(bits) = info.bits_per_sample {
        rows.push(DetailRow {
            label: "Bit depth".into(),
            value: format!("{bits}-bit"),
        });
    }
    if let (Some(data_bytes), Some(byte_rate)) = (info.data_bytes, info.byte_rate) {
        if byte_rate > 0 {
            rows.push(DetailRow {
                label: "Duration".into(),
                value: duration_label(data_bytes as f64 / byte_rate as f64),
            });
        }
    }

    Preview::Details {
        icon: "activity",
        title: "Audio file".into(),
        rows,
    }
}

fn wav_format_label(format: u16) -> &'static str {
    match format {
        1 => "WAV / PCM",
        3 => "WAV / IEEE float",
        6 => "WAV / A-law",
        7 => "WAV / mu-law",
        0xfffe => "WAV / extensible",
        _ => "WAV audio",
    }
}

fn channel_label(channels: u16) -> String {
    match channels {
        1 => "mono".into(),
        2 => "stereo".into(),
        _ => format!("{channels} channels"),
    }
}

fn sample_rate_label(sample_rate: u32) -> String {
    if sample_rate % 1000 == 0 {
        format!("{} kHz", sample_rate / 1000)
    } else if sample_rate % 100 == 0 {
        format!("{:.1} kHz", sample_rate as f64 / 1000.0)
    } else {
        format!("{sample_rate} Hz")
    }
}

fn duration_label(seconds: f64) -> String {
    let total = seconds.round() as u64;
    let minutes = total / 60;
    let seconds = total % 60;
    format!("{minutes}:{seconds:02}")
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

    fn minimal_wav() -> Vec<u8> {
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&40u32.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&44_100u32.to_le_bytes());
        wav.extend_from_slice(&176_400u32.to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&176_400u32.to_le_bytes());
        wav
    }

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

    #[test]
    fn parses_wav_metadata() {
        let info = wav_info(&minimal_wav()).expect("wav info");
        assert_eq!(info.audio_format, Some(1));
        assert_eq!(info.channels, Some(2));
        assert_eq!(info.sample_rate, Some(44_100));
        assert_eq!(info.bits_per_sample, Some(16));
        assert_eq!(info.data_bytes, Some(176_400));
        assert_eq!(sample_rate_label(44_100), "44.1 kHz");
        assert_eq!(duration_label(1.0), "0:01");
    }
}
