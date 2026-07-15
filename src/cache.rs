//! Video cache (`.lcdv` packet format) + recent-media index.
//!
//! Self-contained module: no dependency on other app modules besides the
//! `%LOCALAPPDATA%\AstroshelLcd` convention used elsewhere (see `main.rs`
//! `init_logging`, `config.rs` `config_path`). Uses only crates already in
//! `Cargo.toml` (anyhow, serde, toml, image).

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// `%LOCALAPPDATA%\AstroshelLcd\cache` — created if missing.
pub fn cache_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    let dir = Path::new(&base).join("AstroshelLcd").join("cache");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

// ---------------------------------------------------------------------------
// .lcdv binary format
// ---------------------------------------------------------------------------

const LCDV_MAGIC: &[u8; 5] = b"LCDV1";

/// A cached, pre-encoded video: dimensions/fps + a sequence of H.264 packets
/// with their per-frame display delay in milliseconds.
#[derive(Clone, Debug)]
pub struct CachedVideo {
    pub width: u16,
    pub height: u16,
    pub fps: f32,
    /// (H.264 packet bytes, delay_ms)
    pub frames: Vec<(Vec<u8>, u32)>,
}

/// Write a `CachedVideo` to `path` in the `.lcdv` binary format:
/// `b"LCDV1"` (5 bytes) + `u16 width` (LE) + `u16 height` (LE) + `f32 fps`
/// (LE bits) + `u32 count` (LE), then `count` × (`u32 len` (LE) + `len`
/// packet bytes + `u32 delay_ms` (LE)).
#[allow(dead_code)]
pub fn write_lcdv(path: &Path, v: &CachedVideo) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(path)
        .with_context(|| format!("creating {}", path.display()))?;
    let mut w = BufWriter::new(file);

    w.write_all(LCDV_MAGIC)?;
    w.write_all(&v.width.to_le_bytes())?;
    w.write_all(&v.height.to_le_bytes())?;
    w.write_all(&v.fps.to_le_bytes())?;
    let count = v.frames.len() as u32;
    w.write_all(&count.to_le_bytes())?;
    for (packet, delay_ms) in &v.frames {
        let len = packet.len() as u32;
        w.write_all(&len.to_le_bytes())?;
        w.write_all(packet)?;
        w.write_all(&delay_ms.to_le_bytes())?;
    }
    w.flush()?;
    Ok(())
}

/// Read a `.lcdv` file back into a `CachedVideo`. Bails (no panic) on magic
/// mismatch or truncated/corrupt data.
/// Header size on disk: magic (5) + width (2) + height (2) + fps (4) + count (4).
const LCDV_HEADER_LEN: u64 = 5 + 2 + 2 + 4 + 4;

/// Smallest a single on-disk frame record can be: `u32 len` + `u32 delay_ms`
/// (a zero-length packet is valid, see the roundtrip test).
const LCDV_MIN_FRAME_BYTES: u64 = 4 + 4;

/// Upper bound on the initial `Vec::with_capacity` for the frame list,
/// independent of the (validated, but still attacker/corruption-controlled)
/// `count` field -- keeps a single huge-but-technically-consistent header
/// from provoking a huge up-front allocation.
const LCDV_REASONABLE_CAP: usize = 100_000;

#[allow(dead_code)]
pub fn read_lcdv(path: &Path) -> anyhow::Result<CachedVideo> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut r = BufReader::new(file);

    let mut magic = [0u8; 5];
    r.read_exact(&mut magic)
        .context("reading .lcdv magic (truncated file?)")?;
    if &magic != LCDV_MAGIC {
        bail!("not a .lcdv file (bad magic): {}", path.display());
    }

    let width = read_u16(&mut r)?;
    let height = read_u16(&mut r)?;
    let fps = read_f32(&mut r)?;
    let count = read_u32(&mut r)?;

    // Validate `count` against the file's actual remaining size BEFORE any
    // allocation sized by it: a corrupt/malicious header can claim an
    // arbitrary `count` (e.g. `u32::MAX`), and `Vec::with_capacity` on that
    // directly can abort the process (allocator OOM) rather than return a
    // catchable `Err`. Each frame occupies at least `LCDV_MIN_FRAME_BYTES`
    // bytes on disk, so `count` can never legitimately exceed
    // `remaining_bytes / LCDV_MIN_FRAME_BYTES`.
    let mut remaining_bytes = file_len.saturating_sub(LCDV_HEADER_LEN);
    if count as u64 > remaining_bytes / LCDV_MIN_FRAME_BYTES {
        bail!("corrupt .lcdv: frame count exceeds file size: {}", path.display());
    }

    let mut frames = Vec::with_capacity((count as usize).min(LCDV_REASONABLE_CAP));
    for _ in 0..count {
        let len = read_u32(&mut r)? as usize;
        remaining_bytes = remaining_bytes.saturating_sub(4);
        // Same reasoning as the `count` check above, per-frame: validate the
        // claimed packet `len` against what's actually left in the file
        // before allocating a `Vec` of that size.
        if len as u64 > remaining_bytes {
            bail!("corrupt .lcdv: frame length exceeds remaining file size: {}", path.display());
        }
        let mut packet = vec![0u8; len];
        r.read_exact(&mut packet)
            .context("reading .lcdv packet bytes (truncated file?)")?;
        remaining_bytes = remaining_bytes.saturating_sub(len as u64);
        let delay_ms = read_u32(&mut r)?;
        remaining_bytes = remaining_bytes.saturating_sub(4);
        frames.push((packet, delay_ms));
    }

    Ok(CachedVideo { width, height, fps, frames })
}

fn read_u16<R: Read>(r: &mut R) -> anyhow::Result<u16> {
    let mut buf = [0u8; 2];
    r.read_exact(&mut buf).context("reading u16 (truncated file?)")?;
    Ok(u16::from_le_bytes(buf))
}

fn read_u32<R: Read>(r: &mut R) -> anyhow::Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf).context("reading u32 (truncated file?)")?;
    Ok(u32::from_le_bytes(buf))
}

fn read_f32<R: Read>(r: &mut R) -> anyhow::Result<f32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf).context("reading f32 (truncated file?)")?;
    Ok(f32::from_le_bytes(buf))
}

// ---------------------------------------------------------------------------
// Recent-media index (cache/index.toml)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CacheEntry {
    pub id: String,
    /// "video" | "image" | "gif"
    pub kind: String,
    pub name: String,
    /// `.lcdv` path for a cached video; source path for image/gif.
    pub path: String,
    pub thumb: String,
    pub fps: f32,
    /// Unix seconds.
    pub created: u64,
    pub pinned: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct CacheIndex {
    pub entries: Vec<CacheEntry>,
}

fn index_path() -> PathBuf {
    cache_dir().join("index.toml")
}

/// Load the recent-media index. Missing or corrupt files yield an empty
/// default index rather than panicking.
#[allow(dead_code)]
pub fn load_index() -> CacheIndex {
    match std::fs::read_to_string(index_path()) {
        Ok(s) => toml::from_str(&s).unwrap_or_default(),
        Err(_) => CacheIndex::default(),
    }
}

impl CacheIndex {
    /// Persist the index to `cache_dir()/index.toml`.
    #[allow(dead_code)]
    pub fn save(&self) -> anyhow::Result<()> {
        let path = index_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn add(&mut self, e: CacheEntry) {
        self.entries.push(e);
    }

    #[allow(dead_code)]
    pub fn get(&self, id: &str) -> Option<&CacheEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    #[allow(dead_code)]
    pub fn set_pinned(&mut self, id: &str, pinned: bool) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
            e.pinned = pinned;
        }
    }

    /// Keep the newest `cap` UNPINNED entries (by `created` descending);
    /// delete the files (`path` + `thumb`) of any older unpinned entries
    /// removed (best-effort — I/O errors are ignored) and drop them from
    /// `entries`. Pinned entries are never counted against the cap and are
    /// never removed.
    #[allow(dead_code)]
    pub fn evict_unpinned(&mut self, cap: usize) {
        // Indices of unpinned entries, newest-first by `created`.
        let mut unpinned_idx: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.pinned)
            .map(|(i, _)| i)
            .collect();
        unpinned_idx.sort_by_key(|&i| std::cmp::Reverse(self.entries[i].created));

        let to_remove: std::collections::HashSet<usize> = if unpinned_idx.len() > cap {
            unpinned_idx[cap..].iter().copied().collect()
        } else {
            std::collections::HashSet::new()
        };

        if to_remove.is_empty() {
            return;
        }

        for &i in &to_remove {
            let e = &self.entries[i];
            let _ = std::fs::remove_file(&e.path);
            let _ = std::fs::remove_file(&e.thumb);
        }

        let mut i = 0usize;
        self.entries.retain(|_| {
            let keep = !to_remove.contains(&i);
            i += 1;
            keep
        });
    }

    /// Delete cache entry `id`'s files (`path` + `thumb`, best-effort -- I/O
    /// errors are ignored) and drop it from `entries`, regardless of pinned
    /// state. A no-op if `id` isn't present. Callers that must respect pinning
    /// (e.g. re-import orphan cleanup) should check `get(id).pinned` first.
    #[allow(dead_code)]
    pub fn remove(&mut self, id: &str) {
        if let Some(e) = self.entries.iter().find(|e| e.id == id) {
            let _ = std::fs::remove_file(&e.path);
            let _ = std::fs::remove_file(&e.thumb);
        }
        self.entries.retain(|e| e.id != id);
    }

    /// Delete every unpinned entry's files and drop them from `entries`.
    #[allow(dead_code)]
    pub fn clear_unpinned(&mut self) {
        for e in self.entries.iter().filter(|e| !e.pinned) {
            let _ = std::fs::remove_file(&e.path);
            let _ = std::fs::remove_file(&e.thumb);
        }
        self.entries.retain(|e| e.pinned);
    }
}

/// Monotonic tie-breaker mixed into every `new_id`: guarantees distinct ids
/// even in the (observed) pathological case where the clock's reported
/// resolution doesn't advance between two calls in the same process.
static NEW_ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A filesystem-safe id: unix-seconds + a short hash of `seed` combined with
/// sub-second time (nanoseconds since the epoch, not just whole seconds) and
/// a per-process monotonic counter. Rapid re-imports of the same `seed`
/// (path) within the same second used to collide (same `secs`, same `seed`
/// hash); mixing in nanosecond-resolution time plus a counter makes distinct
/// calls produce distinct ids regardless of import speed.
#[allow(dead_code)]
pub fn new_id(seed: &str) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs();
    let nanos = now.subsec_nanos();
    let seq = NEW_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let mut hasher = DefaultHasher::new();
    seed.hash(&mut hasher);
    nanos.hash(&mut hasher);
    seq.hash(&mut hasher);
    let h = hasher.finish();
    format!("{secs}_{:x}", h & 0xFFFF_FFFF)
}

/// Downscale `img` so its longest side is ~96px and save it as a PNG at
/// `path`.
#[allow(dead_code)]
pub fn write_thumb(path: &Path, img: &image::RgbaImage) -> anyhow::Result<()> {
    const MAX_SIDE: u32 = 96;
    let (w, h) = (img.width().max(1), img.height().max(1));
    let (nw, nh) = if w >= h {
        (MAX_SIDE, (h * MAX_SIDE / w).max(1))
    } else {
        ((w * MAX_SIDE / h).max(1), MAX_SIDE)
    };
    let resized = image::imageops::resize(img, nw, nh, image::imageops::FilterType::Triangle);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    resized
        .save_with_format(path, image::ImageFormat::Png)
        .with_context(|| format!("saving thumb {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_subdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("astroshel_cache_test").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lcdv_roundtrip() {
        let dir = temp_subdir("lcdv_roundtrip");
        let path = dir.join("test.lcdv");

        let v = CachedVideo {
            width: 320,
            height: 172,
            fps: 24.0,
            frames: vec![
                (vec![0xDE, 0xAD, 0xBE, 0xEF], 40),
                (vec![1, 2, 3], 41),
                (vec![], 100), // empty packet is valid too
                (vec![9; 50], 33),
            ],
        };

        write_lcdv(&path, &v).expect("write should succeed");
        let read_back = read_lcdv(&path).expect("read should succeed");

        assert_eq!(read_back.width, v.width);
        assert_eq!(read_back.height, v.height);
        assert_eq!(read_back.fps, v.fps);
        assert_eq!(read_back.frames.len(), v.frames.len());
        for (a, b) in read_back.frames.iter().zip(v.frames.iter()) {
            assert_eq!(a.0, b.0);
            assert_eq!(a.1, b.1);
        }
    }

    #[test]
    fn read_lcdv_on_garbage_bytes_errs() {
        let dir = temp_subdir("garbage");
        let path = dir.join("garbage.lcdv");
        std::fs::write(&path, b"not a real lcdv file at all, just junk bytes").unwrap();

        let result = read_lcdv(&path);
        assert!(result.is_err(), "garbage input should error, not panic");
    }

    #[test]
    fn read_lcdv_on_truncated_file_errs() {
        let dir = temp_subdir("truncated");
        let path = dir.join("truncated.lcdv");
        let v = CachedVideo {
            width: 10,
            height: 10,
            fps: 30.0,
            frames: vec![(vec![1, 2, 3, 4, 5], 33)],
        };
        write_lcdv(&path, &v).unwrap();

        // Truncate the file to cut off mid-frame.
        let full = std::fs::read(&path).unwrap();
        let truncated = &full[..full.len() - 3];
        std::fs::write(&path, truncated).unwrap();

        let result = read_lcdv(&path);
        assert!(result.is_err(), "truncated input should error, not panic");
    }

    #[test]
    fn read_lcdv_missing_file_errs() {
        let dir = temp_subdir("missing");
        let path = dir.join("does_not_exist.lcdv");
        assert!(read_lcdv(&path).is_err());
    }

    #[test]
    fn read_lcdv_huge_frame_count_on_tiny_file_errs_not_aborts() {
        // A well-formed header (correct magic, plausible width/height/fps)
        // but a `count` claiming ~4 billion frames on a file that is only
        // ever going to have a handful of bytes after the header. Before the
        // fix this drove `Vec::with_capacity(count as usize)` directly,
        // which can abort the process (allocator OOM) instead of returning
        // an `Err`. This must return `Err` and must not abort/panic.
        let dir = temp_subdir("huge_count");
        let path = dir.join("huge_count.lcdv");

        let mut bytes = Vec::new();
        bytes.extend_from_slice(LCDV_MAGIC);
        bytes.extend_from_slice(&10u16.to_le_bytes()); // width
        bytes.extend_from_slice(&10u16.to_le_bytes()); // height
        bytes.extend_from_slice(&30.0f32.to_le_bytes()); // fps
        bytes.extend_from_slice(&u32::MAX.to_le_bytes()); // count: corrupt/huge
        std::fs::write(&path, &bytes).unwrap();

        let result = read_lcdv(&path);
        assert!(result.is_err(), "huge frame count on a tiny file should error, not abort");
    }

    #[test]
    fn read_lcdv_frame_len_exceeding_remaining_bytes_errs() {
        // `count` is plausible (1), but the per-frame `len` field claims far
        // more bytes than remain in the file. Must error, not attempt a huge
        // `vec![0u8; len]` allocation.
        let dir = temp_subdir("huge_frame_len");
        let path = dir.join("huge_frame_len.lcdv");

        let mut bytes = Vec::new();
        bytes.extend_from_slice(LCDV_MAGIC);
        bytes.extend_from_slice(&10u16.to_le_bytes());
        bytes.extend_from_slice(&10u16.to_le_bytes());
        bytes.extend_from_slice(&30.0f32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes()); // count = 1
        bytes.extend_from_slice(&u32::MAX.to_le_bytes()); // frame len: corrupt/huge
        std::fs::write(&path, &bytes).unwrap();

        let result = read_lcdv(&path);
        assert!(result.is_err(), "oversized frame len should error, not abort");
    }

    fn make_entry(id: &str, created: u64, pinned: bool, files_dir: &Path) -> CacheEntry {
        let path = files_dir.join(format!("{id}.lcdv"));
        let thumb = files_dir.join(format!("{id}_thumb.png"));
        std::fs::write(&path, b"stub").unwrap();
        std::fs::write(&thumb, b"stub").unwrap();
        CacheEntry {
            id: id.to_string(),
            kind: "video".to_string(),
            name: id.to_string(),
            path: path.to_string_lossy().to_string(),
            thumb: thumb.to_string_lossy().to_string(),
            fps: 24.0,
            created,
            pinned,
        }
    }

    #[test]
    fn evict_unpinned_keeps_cap_newest_unpinned_and_all_pinned() {
        let dir = temp_subdir("evict");
        let mut idx = CacheIndex::default();

        // 12 unpinned entries with distinct `created` timestamps 0..12.
        for i in 0..12u64 {
            idx.add(make_entry(&format!("u{i}"), i, false, &dir));
        }
        // 2 pinned entries, arbitrary timestamps.
        idx.add(make_entry("p0", 5, true, &dir));
        idx.add(make_entry("p1", 100, true, &dir));

        assert_eq!(idx.entries.len(), 14);

        idx.evict_unpinned(10);

        // 10 newest unpinned + both pinned = 12 total.
        assert_eq!(idx.entries.len(), 12);

        // Both pinned entries survive regardless of age.
        assert!(idx.get("p0").is_some());
        assert!(idx.get("p1").is_some());

        // The 10 newest unpinned (created 2..=11) survive; 0 and 1 do not.
        for i in 2..12u64 {
            assert!(idx.get(&format!("u{i}")).is_some(), "u{i} should survive");
        }
        assert!(idx.get("u0").is_none(), "u0 (oldest) should be evicted");
        assert!(idx.get("u1").is_none(), "u1 (2nd oldest) should be evicted");

        // Evicted entries' files were deleted; survivors' files remain.
        assert!(!dir.join("u0.lcdv").exists());
        assert!(!dir.join("u0_thumb.png").exists());
        assert!(!dir.join("u1.lcdv").exists());
        assert!(dir.join("u11.lcdv").exists());
        assert!(dir.join("p0.lcdv").exists());
        assert!(dir.join("p1.lcdv").exists());
    }

    #[test]
    fn evict_unpinned_never_removes_pinned_even_when_over_cap() {
        let dir = temp_subdir("evict_pinned_over_cap");
        let mut idx = CacheIndex::default();
        for i in 0..5u64 {
            idx.add(make_entry(&format!("p{i}"), i, true, &dir));
        }
        idx.evict_unpinned(0);
        assert_eq!(idx.entries.len(), 5, "no pinned entries should ever be removed");
    }

    #[test]
    fn remove_deletes_files_and_entry() {
        let dir = temp_subdir("remove");
        let mut idx = CacheIndex::default();
        idx.add(make_entry("a", 1, false, &dir));
        idx.add(make_entry("b", 2, true, &dir));

        idx.remove("a");

        assert_eq!(idx.entries.len(), 1);
        assert!(idx.get("a").is_none());
        assert!(idx.get("b").is_some());
        assert!(!dir.join("a.lcdv").exists());
        assert!(!dir.join("a_thumb.png").exists());
        assert!(dir.join("b.lcdv").exists());
    }

    #[test]
    fn remove_missing_id_is_a_noop() {
        let dir = temp_subdir("remove_missing");
        let mut idx = CacheIndex::default();
        idx.add(make_entry("a", 1, false, &dir));

        idx.remove("does-not-exist");

        assert_eq!(idx.entries.len(), 1);
        assert!(idx.get("a").is_some());
        assert!(dir.join("a.lcdv").exists());
    }

    #[test]
    fn clear_unpinned_removes_only_unpinned() {
        let dir = temp_subdir("clear_unpinned");
        let mut idx = CacheIndex::default();
        idx.add(make_entry("u0", 1, false, &dir));
        idx.add(make_entry("u1", 2, false, &dir));
        idx.add(make_entry("p0", 3, true, &dir));

        idx.clear_unpinned();

        assert_eq!(idx.entries.len(), 1);
        assert!(idx.get("p0").is_some());
        assert!(!dir.join("u0.lcdv").exists());
        assert!(!dir.join("u1.lcdv").exists());
        assert!(dir.join("p0.lcdv").exists());
    }

    #[test]
    fn cache_index_toml_roundtrip() {
        let dir = temp_subdir("toml_roundtrip");
        let mut idx = CacheIndex::default();
        idx.add(make_entry("a", 1, false, &dir));
        idx.add(make_entry("b", 2, true, &dir));

        let s = toml::to_string(&idx).expect("serialize");
        let back: CacheIndex = toml::from_str(&s).expect("deserialize");

        assert_eq!(back.entries.len(), 2);
        assert_eq!(back.get("a").unwrap().kind, "video");
        assert!(back.get("b").unwrap().pinned);
        assert_eq!(back.get("a").unwrap().created, 1);
        assert_eq!(back.get("b").unwrap().created, 2);
    }

    #[test]
    fn new_id_is_filesystem_safe_and_varies_with_seed() {
        let a = new_id("seed-a");
        let b = new_id("seed-b");
        // Different seeds should (overwhelmingly likely) produce different ids.
        assert_ne!(a, b);
        // No path-hostile characters.
        for c in a.chars().chain(b.chars()) {
            assert!(c.is_ascii_alphanumeric() || c == '_', "unexpected char {c:?} in id");
        }
    }

    #[test]
    fn new_id_same_seed_rapid_reimport_does_not_collide() {
        // Same seed (path), back-to-back calls within the same wall-clock
        // second: before the fix this collided (unix-seconds + hash(seed)
        // only). Sub-second time + a monotonic counter must keep them
        // distinct even for many rapid calls.
        let ids: Vec<String> = (0..50).map(|_| new_id("same/path.mp4")).collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "rapid re-import of the same path must not collide");
    }

    #[test]
    fn write_thumb_downscales_to_max_side_96() {
        let dir = temp_subdir("thumb");
        let path = dir.join("thumb.png");
        let img = image::RgbaImage::from_pixel(640, 320, image::Rgba([10, 20, 30, 255]));
        write_thumb(&path, &img).expect("write_thumb should succeed");

        let loaded = image::open(&path).expect("thumb should be readable").to_rgba8();
        assert_eq!(loaded.width(), 96);
        assert_eq!(loaded.height(), 48);
    }
}
