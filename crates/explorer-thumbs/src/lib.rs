//! On-disk HDR-preserving thumbnail cache.
//!
//! The freedesktop thumbnail spec stores 8-bit sRGB PNGs — which throws
//! away exactly what this explorer exists to show. This cache stores
//! what [`achromat::convert::thumbnail`] produces: linear-light RGBA
//! f16 tagged with primaries and reference luminance, so an HDR
//! thumbnail re-renders bit-identically to a fresh decode.
//!
//! Design constraints, in order:
//!
//! - **Sources are slow (CephFS), the cache is local.** A hit must cost
//!   one local read; a miss costs the decode we were going to do anyway
//!   plus one local write. [`ThumbCache::thumbnail`] runs on a worker
//!   thread, never the UI thread.
//! - **Keys are content-addressed by provenance**, not by trusting the
//!   cache: FNV-1a over (source path, mtime, size, edge, format
//!   version). Touch the file and the old entry is simply never read
//!   again — the byte-budget sweep reclaims it.
//! - **Corruption is a miss, not an error.** Entries are written to a
//!   temp file and renamed into place, and any parse failure on read
//!   deletes the entry and falls through to a fresh decode.
//! - **Native-endian payload.** The cache lives on a local disk and is
//!   rebuildable by construction; it does not try to be portable
//!   between machines.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Context as _;

use achromat::{convert, decode};
use damascene_core::color::{ColorSpace, Primaries, TransferFunction};
use damascene_core::image::{Image, PixelFormat};

/// Bumped whenever the entry encoding changes; part of the key, so old
/// entries age out via the sweep instead of needing migration.
const FORMAT_VERSION: u32 = 1;

const MAGIC: [u8; 4] = *b"PTHM";
const HEADER_LEN: usize = 4 + 4 + 4 + 4 + 1 + 4; // magic, version, w, h, primaries, ref_nits

/// Sanity bound on dimensions parsed from disk (a thumbnail is small by
/// definition; anything bigger is corruption).
const MAX_DIM: u32 = 16_384;

pub struct ThumbCache {
    root: PathBuf,
    edge: u32,
}

/// What a [`ThumbCache::sweep`] found and did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepStats {
    pub files: u64,
    pub bytes: u64,
    pub removed_files: u64,
    pub removed_bytes: u64,
}

impl ThumbCache {
    /// A cache rooted at `root` producing thumbnails with the given
    /// long edge. The directory is created lazily on first store.
    pub fn new(root: PathBuf, edge: u32) -> Self {
        ThumbCache { root, edge }
    }

    /// The conventional location: `$XDG_CACHE_HOME/prism-explorer/thumbs`
    /// (or `~/.cache/…`). `None` when neither variable resolves.
    ///
    /// This is deliberately the *local* cache dir, never the network
    /// filesystem being browsed.
    pub fn standard(edge: u32) -> Option<Self> {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
        Some(Self::new(base.join("prism-explorer/thumbs"), edge))
    }

    /// Thumbnail long edge this cache was built for.
    pub fn edge(&self) -> u32 {
        self.edge
    }

    /// Produce the thumbnail for `src`: cache hit if the source is
    /// unchanged, otherwise decode, downscale in linear light, store,
    /// and return. Worker thread; blocks on IO and decode.
    pub fn thumbnail(&self, src: &Path) -> anyhow::Result<Image> {
        let meta = fs::metadata(src).with_context(|| format!("stat {}", src.display()))?;
        let entry = self.entry_path(src, &meta);

        if let Some(image) = self.load_entry(&entry) {
            return Ok(image);
        }

        let decoded = decode::load_straight(src)?;
        let image = convert::thumbnail(&decoded, self.edge);
        if let Err(e) = self.store_entry(&entry, &image) {
            // A failed store degrades to "no cache", never to an error.
            tracing::warn!(entry = %entry.display(), error = %format!("{e:#}"), "thumb store failed");
        }
        Ok(image)
    }

    /// Cache file for `src` in its current state. Two-level fan-out so
    /// no single directory grows unbounded.
    fn entry_path(&self, src: &Path, meta: &fs::Metadata) -> PathBuf {
        let mut h = Fnv1a::new();
        h.write(&FORMAT_VERSION.to_le_bytes());
        h.write(&self.edge.to_le_bytes());
        h.write(src.as_os_str().as_encoded_bytes());
        if let Ok(mtime) = meta.modified() {
            if let Ok(d) = mtime.duration_since(SystemTime::UNIX_EPOCH) {
                h.write(&d.as_nanos().to_le_bytes());
            }
        }
        h.write(&meta.len().to_le_bytes());
        let hex = format!("{:016x}", h.finish());
        self.root.join(&hex[..2]).join(format!("{hex}.thumb"))
    }

    /// Read and validate one entry. Any mismatch deletes the file and
    /// reports a miss. A hit refreshes the entry's mtime so the sweep's
    /// LRU sees recent use (atime is unreliable under `relatime`).
    fn load_entry(&self, entry: &Path) -> Option<Image> {
        let data = fs::read(entry).ok()?;
        match parse_entry(&data) {
            Some(image) => {
                if let Ok(f) = fs::File::open(entry) {
                    let _ = f.set_modified(SystemTime::now());
                }
                Some(image)
            }
            None => {
                tracing::warn!(entry = %entry.display(), "corrupt thumb entry, removing");
                let _ = fs::remove_file(entry);
                None
            }
        }
    }

    /// Serialize and atomically publish one entry (temp file + rename;
    /// concurrent writers of the same key race benignly).
    fn store_entry(&self, entry: &Path, image: &Image) -> anyhow::Result<()> {
        let data = encode_entry(image)?;
        let dir = entry.parent().expect("entry paths have a parent");
        fs::create_dir_all(dir)?;
        let tmp = entry.with_extension(format!("tmp.{}", std::process::id()));
        fs::write(&tmp, &data)?;
        fs::rename(&tmp, entry)?;
        Ok(())
    }

    /// Delete least-recently-used entries until the cache fits
    /// `budget_bytes`. Also reaps temp files orphaned by a crash.
    /// Worker thread (lowest priority — this is housekeeping).
    pub fn sweep(&self, budget_bytes: u64) -> SweepStats {
        let mut stats = SweepStats::default();
        let mut entries: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
        let Ok(fanout) = fs::read_dir(&self.root) else {
            return stats; // nothing cached yet
        };
        for dir in fanout.flatten() {
            let Ok(files) = fs::read_dir(dir.path()) else {
                continue;
            };
            for f in files.flatten() {
                let path = f.path();
                let Ok(meta) = f.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                // Orphaned temp file: a crash between write and rename.
                if path.extension().is_some_and(|e| e != "thumb") {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                stats.files += 1;
                stats.bytes += meta.len();
                entries.push((path, meta.len(), mtime));
            }
        }
        if stats.bytes <= budget_bytes {
            return stats;
        }
        entries.sort_by_key(|&(_, _, mtime)| mtime);
        let mut remaining = stats.bytes;
        for (path, len, _) in entries {
            if remaining <= budget_bytes {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                remaining -= len;
                stats.removed_files += 1;
                stats.removed_bytes += len;
            }
        }
        tracing::info!(
            files = stats.files,
            mib = stats.bytes >> 20,
            removed = stats.removed_files,
            "thumb cache sweep"
        );
        stats
    }
}

/// Entry layout (all integers little-endian except the pixel payload,
/// which is native-endian f16 bit patterns — see module docs):
///
/// ```text
/// magic "PTHM" · version u32 · width u32 · height u32
/// primaries u8 · reference_luminance_nits f32
/// payload: width × height × 4 × f16
/// ```
///
/// The transfer function is always linear — that is what
/// `achromat::convert::thumbnail` produces; anything else refuses to
/// encode rather than silently mislabeling pixels.
fn encode_entry(image: &Image) -> anyhow::Result<Vec<u8>> {
    let space = image.color_space();
    anyhow::ensure!(
        image.format() == PixelFormat::RgbaF16 && space.transfer == TransferFunction::Linear,
        "thumb entries are linear RgbaF16, got {:?}/{:?}",
        image.format(),
        space.transfer,
    );
    let pixels = image.pixels();
    let mut out = Vec::with_capacity(HEADER_LEN + pixels.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&image.width().to_le_bytes());
    out.extend_from_slice(&image.height().to_le_bytes());
    out.push(encode_primaries(space.primaries));
    out.extend_from_slice(&space.reference_luminance_nits.to_le_bytes());
    out.extend_from_slice(pixels);
    Ok(out)
}

fn parse_entry(data: &[u8]) -> Option<Image> {
    let header = data.get(..HEADER_LEN)?;
    if header[..4] != MAGIC {
        return None;
    }
    let u32_at = |i: usize| u32::from_le_bytes(header[i..i + 4].try_into().unwrap());
    if u32_at(4) != FORMAT_VERSION {
        return None;
    }
    let (width, height) = (u32_at(8), u32_at(12));
    if !(1..=MAX_DIM).contains(&width) || !(1..=MAX_DIM).contains(&height) {
        return None;
    }
    let primaries = decode_primaries(header[16])?;
    let ref_nits = f32::from_le_bytes(header[17..21].try_into().unwrap());
    if !ref_nits.is_finite() || ref_nits <= 0.0 {
        return None;
    }

    let payload = &data[HEADER_LEN..];
    let expected = (width as usize) * (height as usize) * 4;
    if payload.len() != expected * 2 {
        return None;
    }
    let bits: Vec<u16> = payload
        .chunks_exact(2)
        .map(|b| u16::from_ne_bytes([b[0], b[1]]))
        .collect();
    let space = ColorSpace {
        primaries,
        transfer: TransferFunction::Linear,
        reference_luminance_nits: ref_nits,
    };
    Some(Image::from_rgba_f16_bits_in(space, width, height, bits))
}

fn encode_primaries(p: Primaries) -> u8 {
    match p {
        Primaries::Srgb => 0,
        Primaries::DisplayP3 => 1,
        Primaries::Bt2020 => 2,
        Primaries::AdobeRgb => 3,
    }
}

fn decode_primaries(b: u8) -> Option<Primaries> {
    Some(match b {
        0 => Primaries::Srgb,
        1 => Primaries::DisplayP3,
        2 => Primaries::Bt2020,
        3 => Primaries::AdobeRgb,
        _ => return None,
    })
}

/// FNV-1a 64. Stable across runs and platforms (`DefaultHasher` is
/// neither, by contract) and four lines long — not worth a dependency.
struct Fnv1a(u64);

impl Fnv1a {
    fn new() -> Self {
        Fnv1a(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("explorer-thumbs-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_png(path: &Path, w: u32, h: u32, value: u8) {
        let file = fs::File::create(path).unwrap();
        let mut enc = png::Encoder::new(file, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        writer
            .write_image_data(&vec![value; (w * h * 4) as usize])
            .unwrap();
    }

    fn cache_files(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(fanout) = fs::read_dir(root) {
            for d in fanout.flatten() {
                for f in fs::read_dir(d.path()).unwrap().flatten() {
                    out.push(f.path());
                }
            }
        }
        out
    }

    /// Miss → decode + store; hit → identical pixels without touching
    /// the (synthetic) source format; source mtime bump → new key.
    #[test]
    fn round_trip_and_invalidation() {
        let dir = scratch("roundtrip");
        let src = dir.join("photo.png");
        write_png(&src, 64, 32, 200);

        let cache = ThumbCache::new(dir.join("cache"), 16);
        let first = cache.thumbnail(&src).unwrap();
        assert_eq!(first.width().max(first.height()), 16);
        assert_eq!(first.format(), PixelFormat::RgbaF16);
        assert_eq!(cache_files(&cache.root).len(), 1);

        let again = cache.thumbnail(&src).unwrap();
        assert_eq!(again.content_hash(), first.content_hash());
        assert_eq!(cache_files(&cache.root).len(), 1, "hit must not re-store");

        // Same logical content, new mtime + size: stale entry ignored.
        std::thread::sleep(Duration::from_millis(20));
        write_png(&src, 64, 32, 100);
        let fresh = cache.thumbnail(&src).unwrap();
        assert_ne!(fresh.content_hash(), first.content_hash());
        assert_eq!(cache_files(&cache.root).len(), 2);

        fs::remove_dir_all(&dir).unwrap();
    }

    /// A truncated entry is silently removed and re-decoded.
    #[test]
    fn corrupt_entry_is_a_miss() {
        let dir = scratch("corrupt");
        let src = dir.join("photo.png");
        write_png(&src, 32, 32, 50);

        let cache = ThumbCache::new(dir.join("cache"), 16);
        let first = cache.thumbnail(&src).unwrap();

        let entry = cache_files(&cache.root).pop().unwrap();
        let full = fs::read(&entry).unwrap();
        fs::write(&entry, &full[..full.len() / 2]).unwrap();

        let again = cache.thumbnail(&src).unwrap();
        assert_eq!(again.content_hash(), first.content_hash());
        // Re-stored intact after the corrupt one was deleted.
        let entry = cache_files(&cache.root).pop().unwrap();
        assert_eq!(fs::read(&entry).unwrap().len(), full.len());

        fs::remove_dir_all(&dir).unwrap();
    }

    /// The sweep removes oldest-first down to the byte budget and reaps
    /// orphaned temp files.
    #[test]
    fn sweep_evicts_lru() {
        let dir = scratch("sweep");
        let cache = ThumbCache::new(dir.join("cache"), 16);

        let mut entry_bytes = 0;
        for (i, name) in ["a.png", "b.png", "c.png"].iter().enumerate() {
            let src = dir.join(name);
            write_png(&src, 32, 32, i as u8);
            let _ = cache.thumbnail(&src).unwrap();
            let newest = cache_files(&cache.root)
                .into_iter()
                .max_by_key(|p| fs::metadata(p).unwrap().modified().unwrap())
                .unwrap();
            entry_bytes = fs::metadata(&newest).unwrap().len();
            // Distinct mtimes so LRU order is well-defined.
            let f = fs::File::open(&newest).unwrap();
            f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1000 + i as u64))
                .unwrap();
        }
        let fanout = cache_files(&cache.root)[0].parent().unwrap().to_path_buf();
        let orphan = fanout.join("orphan.tmp.123");
        fs::write(&orphan, b"x").unwrap();

        // Budget for exactly two entries: the oldest ("a") goes.
        let stats = cache.sweep(entry_bytes * 2);
        assert_eq!(stats.files, 3);
        assert_eq!(stats.removed_files, 1);
        assert_eq!(cache_files(&cache.root).len(), 2);

        // Under budget: nothing removed.
        let stats = cache.sweep(entry_bytes * 10);
        assert_eq!(stats.removed_files, 0);
        assert!(!orphan.exists(), "sweep reaps orphaned temp files");

        fs::remove_dir_all(&dir).unwrap();
    }
}
