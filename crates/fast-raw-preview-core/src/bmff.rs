//! ISO BMFF / generic-EXIF fallback scanner.
//!
//! Several modern image containers wrap EXIF inside an ISO Base Media File
//! Format (MP4-style box) wrapper rather than a JPEG APP1 segment or a
//! TIFF-at-offset-0 layout:
//!
//!   * Canon CR3       — `ftypcrx ` brand, EXIF lives in a Canon-UUID box
//!                       under `moov` as a sequence of `CMT1`/`CMT2`/...
//!                       sub-boxes whose body is a raw TIFF block.
//!   * HEIC / HEIF     — `ftypmif1`/`ftypheic`/`ftyphevc`, EXIF is an Item
//!                       referenced indirectly via `meta/iinf` + `meta/iloc`
//!                       and stored in `mdat`, prefixed by `Exif\0\0`.
//!   * AVIF            — same wrapper as HEIC, different codec.
//!   * Panasonic RW2   — uses non-standard TIFF magic `0x0055`, but also
//!                       embeds a standard `Exif\0\0II*\0` block midway
//!                       through the file.
//!
//! Rather than write four bespoke walkers, we exploit the fact that all of
//! the above eventually contain a verbatim TIFF block somewhere in the
//! first 256 KB of the file. A linear scan for the two well-known markers
//!
//!   * `Exif\0\0II*\0` / `Exif\0\0MM\0*` — APP1-style EXIF
//!   * `II*\0` / `MM\0*` — bare TIFF magic at non-zero offset
//!
//! reliably finds the right block without parsing box hierarchies. We try
//! each candidate; the first one that produces a `DateTimeOriginal` wins.
//! Cost is bounded by the read window (256 KB) and a handful of TIFF
//! parses, which are themselves O(entries) and bail out quickly on bad
//! inputs.
//!
//! This is deliberately simpler than a full BMFF box-walker; the trade-off
//! is that we can't extract HEIC EXIF when the iloc points outside the
//! 256 KB read window. In practice iPhone-shot HEIC has its EXIF item in
//! the first ~10 KB, well within budget.

use crate::tags::ParsedTags;
use crate::tiff;

/// Scan `data` for embedded EXIF/TIFF blocks and parse the first one that
/// yields a `DateTimeOriginal`. Returns `Some(())` when at least one
/// candidate parsed successfully, `None` when no recognisable EXIF was
/// found in the read window.
pub fn parse_bmff_or_embedded(data: &[u8], out: &mut ParsedTags) -> Option<()> {
  // Pass 1 — `Exif\0\0` markers. These are the canonical APP1-style EXIF
  // block prefix; HEIC/AVIF use them verbatim, RW2's embedded JPEG carries
  // them, and many other containers (PNG eXIf chunk, WebP EXIF chunk) do
  // too. The TIFF block starts immediately after the 6-byte prefix.
  for off in find_all(data, b"Exif\0\0") {
    let start = off + 6;
    if start + 8 > data.len() {
      continue;
    }
    let mut tmp = ParsedTags::default();
    if tiff::parse_tiff_block(&data[start..], &mut tmp).is_some()
      && tmp.date_time_original.is_some()
    {
      merge(out, tmp);
      return Some(());
    }
  }

  // Pass 2 — bare TIFF magic at any non-zero offset. This catches Canon
  // CR3 (CMT1/CMT2 box bodies are raw TIFF blocks at offset ~0x128 and
  // ~0x2b0 respectively), and any other ISO BMFF-wrapped EXIF that
  // doesn't include the `Exif\0\0` prefix.
  //
  // We collect every TIFF-magic position, then merge results greedily —
  // CMT1 carries Make/Model in IFD0, CMT2 carries DateTimeOriginal in
  // its own IFD0 (Canon stores ExifSubIFD entries directly, no SubIFD
  // pointer). Either alone is incomplete.
  let mut found_any = false;
  let mut merged = ParsedTags::default();
  let candidates: Vec<usize> = find_tiff_magic(data);
  for off in candidates {
    if off == 0 {
      // Skip the file-start case — that's the primary parser's job.
      continue;
    }
    let mut tmp = ParsedTags::default();
    if tiff::parse_tiff_block(&data[off..], &mut tmp).is_some() {
      merge_some(&mut merged, tmp);
      found_any = true;
    }
  }
  if found_any && merged.date_time_original.is_some() {
    merge(out, merged);
    return Some(());
  }
  // Fallback: even a CMT1-only hit (Make/Model, no DTO) is worth keeping
  // — the camera-id partition still helps burst grouping when DTO falls
  // back to mtime.
  if found_any && (merged.make.is_some() || merged.model.is_some()) {
    merge(out, merged);
    return Some(());
  }
  None
}

/// Linear scan for every occurrence of `needle` in `hay`. Stops at the
/// first 8 hits — a defensive cap so a pathological file (e.g. a string
/// table full of "Exif") can't blow up our parse budget.
fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
  if needle.is_empty() || hay.len() < needle.len() {
    return Vec::new();
  }
  let mut out = Vec::with_capacity(4);
  let mut i = 0;
  while i + needle.len() <= hay.len() && out.len() < 8 {
    if &hay[i..i + needle.len()] == needle {
      out.push(i);
      i += needle.len();
    } else {
      i += 1;
    }
  }
  out
}

/// Find every plausible TIFF block start: `II*\0` (LE) or `MM\0*` (BE).
/// Caps at 8 hits for the same reason as `find_all`.
fn find_tiff_magic(hay: &[u8]) -> Vec<usize> {
  let mut out = Vec::with_capacity(4);
  let mut i = 0;
  while i + 4 <= hay.len() && out.len() < 8 {
    let h = &hay[i..i + 4];
    if h == [0x49, 0x49, 0x2A, 0x00] || h == [0x4D, 0x4D, 0x00, 0x2A] {
      out.push(i);
    }
    i += 1;
  }
  out
}

/// Move every populated field from `src` into `dst`, overwriting whatever
/// was there. Used when one TIFF block is strictly more informative than
/// the previous candidate.
fn merge(dst: &mut ParsedTags, src: ParsedTags) {
  *dst = src;
}

/// Merge fields from `src` into `dst` only when `dst`'s slot is empty.
/// Used to accumulate Make/Model from CMT1 + DateTimeOriginal from CMT2
/// across separate Canon CR3 sub-boxes.
fn merge_some(dst: &mut ParsedTags, src: ParsedTags) {
  if dst.make.is_none() {
    dst.make = src.make;
  }
  if dst.model.is_none() {
    dst.model = src.model;
  }
  if dst.serial.is_none() {
    dst.serial = src.serial;
  }
  if dst.date_time_original.is_none() {
    dst.date_time_original = src.date_time_original;
  }
  if dst.sub_sec_time_original.is_none() {
    dst.sub_sec_time_original = src.sub_sec_time_original;
  }
  if dst.offset_time_original.is_none() {
    dst.offset_time_original = src.offset_time_original;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn finds_exif_marker_in_padded_buffer() {
    // Synthetic HEIC-shaped buffer: random bytes, then Exif\0\0, then a
    // tiny TIFF block with one DateTimeOriginal entry.
    let mut buf = vec![0u8; 256];
    buf.extend_from_slice(b"Exif\0\0");
    // TIFF header LE, IFD0 at 8, ExifSubIFD pointer to offset 0x1A.
    let tiff_start = buf.len();
    buf.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00]);
    // IFD0 with 1 entry: SubIFD pointer (0x8769)
    buf.extend_from_slice(&[0x01, 0x00]);
    buf.extend_from_slice(&[0x69, 0x87, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00,
                            0x1A, 0x00, 0x00, 0x00]);
    buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // next-IFD = 0
    // ExifSubIFD at offset 0x1A within the TIFF block:
    // 1 entry, DateTimeOriginal, ASCII, count=20, offset to string.
    buf.extend_from_slice(&[0x01, 0x00]);
    let dto_string_off_within_tiff: u32 = 0x1A + 2 + 12 + 4;
    buf.extend_from_slice(&[0x03, 0x90, 0x02, 0x00,
                            0x14, 0x00, 0x00, 0x00]);
    buf.extend_from_slice(&dto_string_off_within_tiff.to_le_bytes());
    buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // next-IFD = 0
    buf.extend_from_slice(b"2025:01:02 03:04:05\0");
    let _ = tiff_start; // silence unused

    let mut tags = ParsedTags::default();
    parse_bmff_or_embedded(&buf, &mut tags).expect("should find embedded EXIF");
    assert_eq!(tags.date_time_original.as_deref(), Some("2025:01:02 03:04:05"));
  }

  #[test]
  fn returns_none_when_no_exif_present() {
    let buf = vec![0xAAu8; 1024];
    let mut tags = ParsedTags::default();
    assert!(parse_bmff_or_embedded(&buf, &mut tags).is_none());
  }

  #[test]
  fn skips_offset_zero_to_avoid_double_parsing_primary_block() {
    // Buffer that starts with a valid TIFF magic at offset 0 — the bmff
    // fallback must not claim it (the primary tiff::parse_tiff_block
    // handler owns offset-0 cases).
    let mut buf = vec![0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00];
    buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    let mut tags = ParsedTags::default();
    assert!(parse_bmff_or_embedded(&buf, &mut tags).is_none());
  }
}
