//! shotselect-exif · in-process EXIF reader for ShotSelect's burst-grouping
//! hot path. Exposes one napi-rs function: `parseBurstMetadata(paths) →
//! BurstMeta[]`. See PLAN.md for design + perf targets.

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(feature = "napi")]
#[macro_use]
extern crate napi_derive;

use std::path::Path;

use memmap2::Mmap;
#[cfg(any(feature = "napi", test))]
use rayon::prelude::*;

mod bmff;
mod jpeg;
mod preview;
mod raf;
mod tags;
mod tiff;

pub use tags::ParsedTags;

/// Maximum bytes we mmap+read for one file. EXIF spec caps APP1 at 64 KB.
/// RAF files have larger headers but the embedded JPEG offset always points
/// within the first ~256 KB; we cap higher to be safe.
const READ_PEEK_BYTES: usize = 1 << 18; // 256 KB

/// Result row returned by the parser. Same fields whether called from
/// napi or from pure Rust.
#[cfg_attr(feature = "napi", napi(object))]
#[cfg_attr(not(feature = "napi"), derive(Debug, Clone))]
pub struct BurstMeta {
  pub path: String,
  /// ms since epoch, or `None` when DateTimeOriginal absent. Caller falls
  /// back to filesystem mtime in that case.
  pub date_time_original_ms: Option<f64>,
  /// `Make|Model|Serial`, `Make|Model`, or `Make`. None when no EXIF Make.
  pub camera_id: Option<String>,
}

/// Public entry point. Reads up to 256 KB of every input file in parallel
/// and parses EXIF tags relevant to burst grouping. Best-effort: a failed
/// parse returns an empty BurstMeta for that path rather than aborting.
///
/// The path order in the output matches the input order.
#[cfg(feature = "napi")]
#[napi]
pub fn parse_burst_metadata(paths: Vec<String>) -> Vec<BurstMeta> {
  with_scoped_pool(
    || paths.par_iter().map(|p| parse_one(p).unwrap_or_else(|_| empty_meta(p))).collect(),
    || paths.iter().map(|p| parse_one(p).unwrap_or_else(|_| empty_meta(p))).collect(),
  )
}

/// The addon is loaded by disposable Node workers. Rayon's global pool outlives
/// their environment and can keep executing code after Windows unloads the DLL.
/// Scoped threads are joined before returning to N-API, including on panic.
/// If a pool cannot be created, keep the parser's best-effort contract serially.
#[cfg(any(feature = "napi", test))]
fn with_scoped_pool<R: Send>(parallel: impl FnOnce() -> R + Send, serial: impl FnOnce() -> R) -> R {
  rayon::ThreadPoolBuilder::new()
    .build_scoped(|thread| thread.run(), |pool| pool.install(parallel))
    .unwrap_or_else(|_| serial())
}

/// Single-file synchronous variant — useful for benchmarking and the rare
/// caller that wants one file's metadata without spinning up a worker pool.
#[cfg(feature = "napi")]
#[napi]
pub fn parse_burst_metadata_one(path: String) -> BurstMeta {
  parse_one(&path).unwrap_or_else(|_| empty_meta(&path))
}

// ─── Internal ────────────────────────────────────────────────────────────

/// True when `buf` looks like an ISO Base Media File Format container
/// (MP4-shaped). The signature is the `ftyp` box at offset 4: bytes 0..3
/// are the box size (BE u32, must be ≥ 16) and bytes 4..7 are the literal
/// ASCII `ftyp`. We don't validate the brand — `crx ` (CR3), `heic`,
/// `heix`, `mif1`, `avif`, `qt  ` all funnel through the same scanner.
fn is_iso_bmff(buf: &[u8]) -> bool {
  buf.len() >= 12 && &buf[4..8] == b"ftyp"
}

fn empty_meta(p: &str) -> BurstMeta {
  BurstMeta {
    path: p.to_string(),
    date_time_original_ms: None,
    camera_id: None,
  }
}

fn parse_one(path: &str) -> std::io::Result<BurstMeta> {
  // mmap the file and only touch the first READ_PEEK_BYTES. The OS pages
  // in the prefix lazily and never reads the full RAW.
  let file = std::fs::File::open(Path::new(path))?;
  let len = file.metadata()?.len() as usize;
  let read_len = len.min(READ_PEEK_BYTES);
  if read_len < 8 {
    // Too small to be a real image.
    return Ok(empty_meta(path));
  }
  // SAFETY: the file is opened above and we only READ from it; no
  // mutation, no aliasing concern within this scope.
  let mmap = unsafe { Mmap::map(&file)? };
  let buf = &mmap[..read_len];
  let mut tags = ParsedTags::default();
  let parsed = if buf.starts_with(b"FUJIFILMCCD-RAW ") {
    // Fujifilm RAF · custom header pointing at an embedded JPEG that
    // carries the EXIF block.
    raf::parse_raf(buf, &mut tags)
  } else if buf.len() >= 2 && buf[0] == 0xFF && buf[1] == 0xD8 {
    // Standard JPEG · APP1/Exif segment carries the TIFF block.
    jpeg::parse_jpeg(buf, &mut tags)
  } else if is_iso_bmff(buf) {
    // ISO BMFF (MP4-shaped) container · Canon CR3, HEIC/HEIF, AVIF.
    // EXIF is buried inside `meta/iinf+iloc` (HEIC) or a Canon-UUID
    // sub-box (CR3). Both layouts ultimately contain a verbatim TIFF
    // block somewhere in the read window — bmff::parse_bmff_or_embedded
    // scans for it without walking the full box hierarchy.
    bmff::parse_bmff_or_embedded(buf, &mut tags)
  } else if buf.len() >= 4 && (&buf[..4] == b"II*\0" || &buf[..4] == b"MM\0*") {
    // Plain TIFF container · the EXIF tags live in IFD0 + the EXIF
    // SubIFD. This single branch covers most major RAW formats:
    //   • Sony ARW       (II*\0)
    //   • Nikon NEF      (II*\0 or MM\0* depending on body)
    //   • Canon CR2      (II*\0) — legacy TIFF-based; CR3 is ISOBMFF
    //   • Olympus ORF    (II*\0)
    //   • Adobe DNG      (II*\0 / MM\0*)
    //   • Pentax PEF     (II*\0)
    // Camera-specific MakerNotes are NOT walked; we only care about
    // IFD0 + ExifSubIFD entries which are spec-stable.
    let primary = tiff::parse_tiff_block(buf, &mut tags);
    // Some flavours (e.g. multi-page TIFF with MakerNote-only DTO)
    // populate Make/Model in IFD0 but stash DateTimeOriginal in an
    // embedded `Exif\0\0` block — fall back to the scanner when DTO
    // is missing after the primary parse.
    if tags.date_time_original.is_none() {
      let _ = bmff::parse_bmff_or_embedded(buf, &mut tags);
    }
    primary
  } else if buf.len() >= 4 && &buf[..2] == b"II" && buf[2] == 0x55 && buf[3] == 0x00 {
    // Panasonic RW2 · non-standard TIFF magic 0x0055. The IFD0 layout
    // is otherwise identical to a standard TIFF, but RW2 also embeds
    // a full `Exif\0\0` APP1-style block midway through the file that
    // carries DateTimeOriginal. Use the scanner as the primary parser
    // here — the IFD0 in the non-standard header is camera-specific
    // and its tags collide with stock EXIF tag IDs.
    bmff::parse_bmff_or_embedded(buf, &mut tags)
  } else {
    // Format we don't recognise yet. Caller falls back to mtime; the
    // IPC layer falls through to exiftool for any uncovered file in
    // the same batch.
    None
  };
  if parsed.is_none() {
    return Ok(empty_meta(path));
  }
  Ok(BurstMeta {
    path: path.to_string(),
    date_time_original_ms: tags.date_time_original_ms(),
    camera_id: tags.camera_id(),
  })
}

/// Result of an embedded-preview extraction.
#[cfg_attr(feature = "napi", napi(object))]
#[cfg_attr(not(feature = "napi"), derive(Debug, Clone))]
pub struct PreviewResult {
  /// Embedded JPEG bytes (length 0 when nothing was found). napi-rs
  /// converts this into a Node Buffer on the JS side; cheap to pass.
  pub jpeg: Vec<u8>,
  /// Source identifier · `tiff-ifd`, `cr3-prvw`, or empty when no
  /// preview was found. Mirrors the diagnostic strings in
  /// `electron/main.ts`'s `X-Thumb` response header so the JS path
  /// can attribute its perf-log entries cleanly.
  pub source: String,
}

/// Extract the largest embedded preview JPEG from a RAW / CR3 file.
/// Currently dispatches to:
///   - the TIFF/IFD walker for ARW, NEF, CR2, DNG, ORF, PEF, 3FR,
///     RW2 (and any other TIFF-magic RAW that puts a JPEG in IFD1
///     or a SubIFD via tags 0x0201/0x0202)
///   - the ISO BMFF walker for Canon CR3 (uuid/prvw box)
///
/// JPEGs go directly to the renderer (it already has
/// `extractExifThumbnail` on the main side) and HEIC needs libheif
/// which we don't ship · those paths return an empty buffer here.
///
/// We mmap the whole file because preview JPEGs can live anywhere
/// (CR2 puts them at the END of the file, sometimes 30 MB past
/// IFD0). This is unlike `parse_burst_metadata` which reads only
/// the first 256 KB.
#[cfg(feature = "napi")]
#[napi]
pub fn extract_embedded_preview(path: String) -> PreviewResult {
  match extract_preview_inner(&path) {
    Some((bytes, source)) => PreviewResult {
      jpeg: bytes,
      source: source.to_string(),
    },
    None => PreviewResult {
      jpeg: Vec::new(),
      source: String::new(),
    },
  }
}

/// Parallel-batch variant for large folders. Same contract as
/// `extract_embedded_preview` per element. Errors per file are
/// surfaced as empty-jpeg rows; the batch never aborts on one
/// bad file. Output preserves input order.
#[cfg(feature = "napi")]
#[napi]
pub fn extract_embedded_previews(paths: Vec<String>) -> Vec<PreviewResult> {
  let extract = |p: &String| match extract_preview_inner(p) {
    Some((bytes, source)) => PreviewResult {
      jpeg: bytes,
      source: source.to_string(),
    },
    None => PreviewResult {
      jpeg: Vec::new(),
      source: String::new(),
    },
  };
  with_scoped_pool(
    || paths.par_iter().map(extract).collect(),
    || paths.iter().map(extract).collect(),
  )
}

#[cfg(test)]
mod worker_lifetime_tests {
  use super::*;
  use std::cell::RefCell;
  use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};

  struct ExitCounter(Arc<AtomicUsize>);
  impl Drop for ExitCounter {
    fn drop(&mut self) {
      self.0.fetch_add(1, Ordering::SeqCst);
    }
  }
  thread_local! { static EXIT_COUNTER: RefCell<Option<ExitCounter>> = const { RefCell::new(None) }; }

  #[test]
  fn batch_returns_only_after_native_workers_exit() {
    let started = Arc::new(AtomicUsize::new(0));
    let exited = Arc::new(AtomicUsize::new(0));
    let rows = with_scoped_pool(
      || {
        (0..128).into_par_iter().map(|value| {
          EXIT_COUNTER.with(|counter| {
            if counter.borrow().is_none() {
              started.fetch_add(1, Ordering::SeqCst);
              *counter.borrow_mut() = Some(ExitCounter(exited.clone()));
            }
          });
          value
        }).collect::<Vec<_>>()
      },
      || panic!("the regression requires a native pool"),
    );
    assert_eq!(rows, (0..128).collect::<Vec<_>>());
    assert!(started.load(Ordering::SeqCst) > 0);
    assert_eq!(
      exited.load(Ordering::SeqCst), started.load(Ordering::SeqCst),
      "native threads must exit before the addon can unload",
    );
  }
}

fn extract_preview_inner(path: &str) -> Option<(Vec<u8>, &'static str)> {
  let file = std::fs::File::open(Path::new(path)).ok()?;
  let len = file.metadata().ok()?.len() as usize;
  if len < 16 {
    return None;
  }
  // SAFETY: read-only mmap; no aliasing concerns inside this scope.
  let mmap = unsafe { Mmap::map(&file).ok()? };
  let head = &mmap[..16.min(mmap.len())];
  if head.len() >= 8 && &head[4..8] == b"ftyp" {
    // ISO BMFF · CR3 uses `crx ` brand. HEIC uses `heic`/`mif1`/`hevc`
    // — those need libheif and aren't supported by this extractor;
    // we only attempt CR3.
    return preview::find_cr3_preview(&mmap[..]).map(|p| (p.bytes.to_vec(), p.source));
  }
  // TIFF-based RAW (II*\0 / MM\0* / IIU\0 for Panasonic RW2).
  if head.len() >= 4 {
    let m = &head[..4];
    if m == b"II*\0" || m == b"MM\0*" || (m[0] == 0x49 && m[1] == 0x49 && m[2] == 0x55 && m[3] == 0x00) {
      return preview::find_tiff_preview(&mmap[..]).map(|p| (p.bytes.to_vec(), p.source));
    }
  }
  // RAF (Fuji) · the Rust reader is SLOWER than exiftool on slow exfat
  // SD card readers (mmap + 5 MB read = ~1-3 s/file vs exiftool's
  // ~25-100 ms streaming reads). Keep the dispatch out and fall through
  // to exiftool-vendored on the JS side. Internal-disk RAF reads ARE
  // fast in Rust (~0.5 ms) but the demo's hot path is the SD card. See
  // docs/engineering/perf-log.md (2026-05-10) for the receipts.
  //
  // JPEG / HEIC also out of scope: JPEGs have `extractExifThumbnail`
  // already; HEIC needs libheif which we don't ship.
  None
}

// ─── Internal helper exposed for the bench binary ────────────────────────
// The bench harness needs a non-napi entry point so it can be invoked from
// `cargo run --bin bench` without going through the Node bridge.
#[doc(hidden)]
pub fn parse_one_for_bench(path: &str) -> Option<BurstMeta> {
  parse_one(path).ok()
}

#[doc(hidden)]
pub fn extract_preview_for_bench(path: &str) -> Option<(Vec<u8>, &'static str)> {
  extract_preview_inner(path)
}
