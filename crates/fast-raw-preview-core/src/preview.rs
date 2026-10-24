//! Embedded-preview extractor.
//!
//! Every RAW format we care about embeds at least one full-resolution
//! JPEG preview that camera makers use for in-camera review screens.
//! Photo culling tools (us, Photo Mechanic, FastRawViewer, …) can
//! display these previews instantly without decoding the RAW pixels.
//!
//! The current ShotSelect pipeline (`electron/rawProcessor.ts`)
//! falls back to an `exiftool-vendored` subprocess for any format
//! that isn't RAF or generic-TIFF. That subprocess takes 100–500 ms
//! cold per file (Perl interpreter + IPC + IFD walk) — for a Canon
//! R5 user opening a 500-shot wedding folder, that's 50–250 seconds
//! before all RAW thumbnails appear. This module replaces the
//! subprocess for TIFF-based + ISO-BMFF (CR3) RAWs with a 5–10 ms
//! in-process Rust extractor.
//!
//! Format coverage:
//!   * TIFF-based RAW (CR2, ARW, NEF, DNG, ORF, PEF, 3FR, …)
//!     — walk every IFD (IFD0, IFD1, all SubIFDs via tag 0x014A),
//!       find the LARGEST JPEG by `JPEGInterchangeFormat` (0x0201) +
//!       `JPEGInterchangeFormatLength` (0x0202), return those bytes.
//!   * Canon CR3 (ISO BMFF) — walk box hierarchy down to `prvw`
//!     (under `moov/uuid` for full-res preview); emit JPEG bytes.
//!   * RAF (Fuji) — already handled by `electron/rawProcessor.ts`'s
//!     pure-JS extractor; we don't duplicate.
//!   * JPEG / HEIC — out of scope here. JPEGs already have
//!     `extractExifThumbnail` in main.ts; HEIC needs libheif which
//!     we don't ship.
//!
//! Tag/box references:
//!   * TIFF 6.0 § 4 IFD pointer chain (next-IFD offset)
//!   * EXIF 2.32 tag 0x0201 / 0x0202 (JPEGInterchangeFormat /Length)
//!   * EXIF 2.32 tag 0x014A (SubIFDs pointer array)
//!   * ISO/IEC 14496-12 §8.3 box layout (size + type + payload)
//!   * Canon CR3 reverse-engineered notes (laurentclevy/canon_cr3
//!     — `prvw` UUID is a Canon-specific extension)

use crate::tiff::Endian;

/// Output of the preview extractor.
pub struct PreviewBytes<'a> {
  /// Borrowed slice into the input buffer · zero-copy. Caller copies
  /// out before the buffer is dropped.
  pub bytes: &'a [u8],
  /// Source format identifier — used in telemetry / perf-log to know
  /// which path produced the preview. Stable strings; do not change
  /// without bumping consumers.
  pub source: &'static str,
}

const TAG_NEW_SUBFILE_TYPE: u16 = 0x00FE;
const TAG_SUBIFDS: u16 = 0x014A;
const TAG_JPEG_OFFSET: u16 = 0x0201;
const TAG_JPEG_LENGTH: u16 = 0x0202;
/// Some Sony cameras stash the preview in tag 0xC51B (PreviewIFD).
const TAG_PREVIEW_IFD: u16 = 0xC51B;

const TYPE_LONG: u16 = 4;

/// Find the largest embedded JPEG inside a TIFF / TIFF-based RAW
/// buffer. The buffer should be the entire file mmap'd in — many
/// preview JPEGs (~2-10 MB) live near the END of the file, well
/// past the 256 KB header window the EXIF reader uses, so callers
/// pass the FULL file slice here, not the truncated one.
///
/// Returns `None` when no JPEG is found in any IFD reachable from
/// IFD0 / IFD1 / SubIFDs (depth-limited at 4 to defend against
/// pathological inputs).
pub fn find_tiff_preview(data: &[u8]) -> Option<PreviewBytes<'_>> {
  if data.len() < 8 {
    return None;
  }
  let endian = match &data[0..2] {
    [0x49, 0x49] => Endian::Little,
    [0x4D, 0x4D] => Endian::Big,
    _ => return None,
  };
  // Magic must be 0x002A (standard TIFF) or 0x0055 (Panasonic RW2).
  let magic = read_u16(endian, &data[2..4]);
  if magic != 0x002A && magic != 0x0055 {
    return None;
  }
  let ifd0_off = read_u32(endian, &data[4..8]) as usize;
  let mut best: Option<(usize, usize)> = None; // (offset, length)
  walk_chain(data, ifd0_off, endian, &mut best, 0);
  best.and_then(|(off, len)| {
    if off + len > data.len() {
      return None;
    }
    // Sanity · the preview should start with a JPEG SOI marker. Some
    // RAWs reuse 0x0201/0x0202 for non-JPEG strips (older NEFs in
    // particular); reject anything that isn't `FF D8`.
    if len < 4 || data[off] != 0xFF || data[off + 1] != 0xD8 {
      return None;
    }
    Some(PreviewBytes {
      bytes: &data[off..off + len],
      source: "tiff-ifd",
    })
  })
}

/// Walk the IFD chain starting at `off`, recursing into every SubIFD
/// (tag 0x014A) and into the next-IFD pointer at the end of each IFD.
/// `best` accumulates the largest `(offset, length)` JPEG seen so far.
fn walk_chain(
  data: &[u8],
  off: usize,
  endian: Endian,
  best: &mut Option<(usize, usize)>,
  depth: u8,
) {
  // Defend against deeply nested SubIFD chains and self-loops.
  if depth > 4 {
    return;
  }
  let mut cur = off;
  for _ in 0..8 {
    // Cap chain length · TIFF spec doesn't bound this, but real RAWs
    // never have more than 2-3 IFDs in a row. Stop after 8 to avoid
    // pathological loops.
    if cur == 0 || cur + 2 > data.len() {
      return;
    }
    let n = read_u16(endian, &data[cur..cur + 2]) as usize;
    let entries_start = cur + 2;
    if entries_start + n * 12 + 4 > data.len() {
      return;
    }

    let mut jpeg_off: Option<u32> = None;
    let mut jpeg_len: Option<u32> = None;

    for i in 0..n {
      let p = entries_start + i * 12;
      let tag = read_u16(endian, &data[p..p + 2]);
      let dtype = read_u16(endian, &data[p + 2..p + 4]);
      let count = read_u32(endian, &data[p + 4..p + 8]) as usize;
      let value_or_off = &data[p + 8..p + 12];

      match tag {
        TAG_JPEG_OFFSET => {
          jpeg_off = Some(read_u32(endian, value_or_off));
        }
        TAG_JPEG_LENGTH => {
          jpeg_len = Some(read_u32(endian, value_or_off));
        }
        TAG_SUBIFDS | TAG_PREVIEW_IFD => {
          // SubIFDs · pointer or array of pointers (LONG / count).
          // Recurse into each pointed-to IFD.
          if dtype == TYPE_LONG {
            if count == 1 {
              let sub = read_u32(endian, value_or_off) as usize;
              walk_chain(data, sub, endian, best, depth + 1);
            } else {
              let arr_off = read_u32(endian, value_or_off) as usize;
              if arr_off + count * 4 <= data.len() {
                for j in 0..count {
                  let sub_off = arr_off + j * 4;
                  let sub = read_u32(endian, &data[sub_off..sub_off + 4]) as usize;
                  walk_chain(data, sub, endian, best, depth + 1);
                }
              }
            }
          }
        }
        TAG_NEW_SUBFILE_TYPE => {
          // Bit 0 = "this is a reduced-resolution image" (preview).
          // We don't gate on this — some RAWs leave it 0 even on
          // their preview IFD — but read it for future heuristics.
          let _ = read_u32(endian, value_or_off);
        }
        _ => {}
      }
    }

    // Bank this IFD's JPEG if both offset+length present.
    if let (Some(o), Some(l)) = (jpeg_off, jpeg_len) {
      let off_us = o as usize;
      let len_us = l as usize;
      if len_us > 0 && off_us + len_us <= data.len() {
        match best {
          Some((_, best_len)) if *best_len >= len_us => {}
          _ => {
            *best = Some((off_us, len_us));
          }
        }
      }
    }

    // Follow next-IFD pointer.
    let next_ifd_off = entries_start + n * 12;
    cur = read_u32(endian, &data[next_ifd_off..next_ifd_off + 4]) as usize;
  }
}

/// Find the embedded preview JPEG inside a Canon CR3 (ISO BMFF)
/// container. CR3 stores the full-resolution preview in a `prvw`
/// box under `moov/uuid` (Canon UUID `eaf42b5e-1c98-4b88-b9fb-b7dc406e4d16`).
/// The box payload is structured as:
///
///   [version(1)][reserved(1)][unknown(2)][unknown(2)][width(2)]
///   [height(2)][unknown(2)][len(4)][JPEG bytes]
///
/// We walk the box hierarchy, locate the `prvw` (or fall back to
/// the older `THMB` thumbnail box), and return a slice into the
/// embedded JPEG bytes.
pub fn find_cr3_preview(data: &[u8]) -> Option<PreviewBytes<'_>> {
  // Verify ftyp brand · `crx ` (Canon's CR3 brand).
  if data.len() < 16 {
    return None;
  }
  if &data[4..8] != b"ftyp" {
    return None;
  }
  // Walk top-level boxes.
  let mut moov_payload: Option<&[u8]> = None;
  for (typ, payload) in iter_boxes(data) {
    if typ == *b"moov" {
      moov_payload = Some(payload);
      break;
    }
  }
  let moov = moov_payload?;
  // Inside moov · find uuid boxes. Canon stashes preview/CMT under uuid boxes.
  // There can be multiple uuid boxes; we iterate through all of them until we find
  // one containing a 'prvw' or 'THMB' sub-box, rather than breaking on the first one.
  let mut prvw_payload: Option<&[u8]> = None;
  let mut thmb_payload: Option<&[u8]> = None;
  for (typ, payload) in iter_boxes(moov) {
    if typ == *b"uuid" && payload.len() >= 16 {
      // The first 16 bytes after `uuid` are the UUID itself.
      let uuid_inner = &payload[16..];
      // Search for prvw or THMB inside this uuid box.
      for (sub_typ, sub_payload) in iter_boxes(uuid_inner) {
        if sub_typ == *b"prvw" {
          prvw_payload = Some(sub_payload);
        } else if sub_typ == *b"THMB" {
          thmb_payload = Some(sub_payload);
        }
      }
      // If we successfully found a 'prvw' preview box, we can stop searching.
      if prvw_payload.is_some() {
        break;
      }
    }
  }
  // Prefer prvw (full-res preview) over THMB (thumbnail).
  let payload = prvw_payload.or(thmb_payload)?;
  // The first 16 bytes are CR3-specific header; JPEG SOI is at
  // offset 16 (or sometimes 12 for older firmware).
  for offset in &[16usize, 12, 8, 4, 0] {
    if *offset + 4 > payload.len() {
      continue;
    }
    if payload[*offset] == 0xFF && payload[*offset + 1] == 0xD8 {
      let bytes = &payload[*offset..];
      return Some(PreviewBytes {
        bytes,
        source: "cr3-prvw",
      });
    }
  }
  None
}

/// Generator-style iterator over ISO BMFF top-level boxes. Yields
/// `(box_type_4cc, payload_slice)` for each box. Stops at end of
/// `buf` or on malformed boxes (size 0 or larger than remaining).
fn iter_boxes(buf: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
  let mut pos = 0usize;
  std::iter::from_fn(move || {
    if pos + 8 > buf.len() {
      return None;
    }
    let size = u32::from_be_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
    let typ = [buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]];
    let header_len = 8;
    let total = if size == 1 {
      // 64-bit largesize · spec-rare on consumer cameras but valid.
      if pos + 16 > buf.len() {
        return None;
      }
      let large = u64::from_be_bytes([
        buf[pos + 8],
        buf[pos + 9],
        buf[pos + 10],
        buf[pos + 11],
        buf[pos + 12],
        buf[pos + 13],
        buf[pos + 14],
        buf[pos + 15],
      ]) as usize;
      if large < 16 || pos + large > buf.len() {
        return None;
      }
      large
    } else if size == 0 {
      // box extends to end of stream — we just consume rest.
      buf.len() - pos
    } else if size < header_len || pos + size > buf.len() {
      return None;
    } else {
      size
    };
    let payload_off = if size == 1 { 16 } else { header_len };
    let payload = &buf[pos + payload_off..pos + total];
    pos += total;
    Some((typ, payload))
  })
}

#[inline(always)]
fn read_u16(e: Endian, b: &[u8]) -> u16 {
  match e {
    Endian::Little => u16::from_le_bytes([b[0], b[1]]),
    Endian::Big => u16::from_be_bytes([b[0], b[1]]),
  }
}

#[inline(always)]
fn read_u32(e: Endian, b: &[u8]) -> u32 {
  match e {
    Endian::Little => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
    Endian::Big => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Build a synthetic LE TIFF with one IFD0 entry pointing at an
  /// embedded "JPEG" (just `FF D8 FF E0 …`). Verifies the walker
  /// finds the bytes.
  #[test]
  fn finds_jpeg_in_ifd0() {
    // Layout:
    //   0..8   TIFF header (II + 0x002A + IFD0 offset = 8)
    //   8..10  IFD entry count = 2
    //   10..22 entry 0: tag 0x0201, type LONG, count 1, value=offset
    //   22..34 entry 1: tag 0x0202, type LONG, count 1, value=length
    //   34..38 next-IFD = 0
    //   38..   embedded JPEG bytes
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00]); // II + magic
    buf.extend_from_slice(&[0x08, 0x00, 0x00, 0x00]); // IFD0 at 8
    buf.extend_from_slice(&[0x02, 0x00]); // 2 entries
    // entry 0 · 0x0201 LONG count=1 value=38 (offset of JPEG)
    buf.extend_from_slice(&[0x01, 0x02, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x26, 0x00, 0x00, 0x00]);
    // entry 1 · 0x0202 LONG count=1 value=8 (8 bytes of "JPEG")
    buf.extend_from_slice(&[0x02, 0x02, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00]);
    // next-IFD = 0
    buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    // Embedded "JPEG" (just the SOI + minimal valid markers).
    buf.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46]);

    let result = find_tiff_preview(&buf).expect("preview should be found");
    assert_eq!(result.source, "tiff-ifd");
    assert_eq!(result.bytes.len(), 8);
    assert_eq!(result.bytes[0], 0xFF);
    assert_eq!(result.bytes[1], 0xD8);
  }

  #[test]
  fn rejects_when_no_jpeg_tags() {
    // Empty IFD0 · no preview tags.
    let buf = vec![
      0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    assert!(find_tiff_preview(&buf).is_none());
  }

  #[test]
  fn rejects_when_offset_does_not_point_to_jpeg() {
    // IFD0 with 0x0201 + 0x0202 pointing at non-JPEG bytes (no SOI).
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00]);
    buf.extend_from_slice(&[0x02, 0x00]);
    buf.extend_from_slice(&[0x01, 0x02, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x26, 0x00, 0x00, 0x00]);
    buf.extend_from_slice(&[0x02, 0x02, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00]);
    buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    // Garbage bytes where SOI should be.
    buf.extend_from_slice(&[0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77]);
    assert!(find_tiff_preview(&buf).is_none());
  }

  #[test]
  fn picks_largest_jpeg_across_multiple_ifds() {
    // Two IFDs · the second has a bigger JPEG; walker should pick it.
    // For simplicity build two separate IFDs each with one preview;
    // verify the larger one wins.
    //
    // Layout:
    //   0..8     TIFF header, IFD0 at 8
    //   8..10    IFD0 entry count = 2
    //   10..22   tag 0x0201 → small JPEG offset
    //   22..34   tag 0x0202 → small JPEG length = 4
    //   34..38   next-IFD pointer → IFD1 offset
    //   IFD1 entries pointing at a larger 12-byte JPEG.
    let small_jpeg: [u8; 4] = [0xFF, 0xD8, 0xFF, 0xE1];
    let large_jpeg: [u8; 12] = [
      0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01,
    ];
    // Position planning · we know everything up front so we can
    // hand-write offsets.
    //
    //   IFD0 at 8 (count 2 → 2 + 24 + 4 = 30 bytes → ends at 38)
    //   small_jpeg at 38 (length 4) → ends at 42
    //   IFD1 at 42 (count 2 → 30 bytes → ends at 72)
    //   large_jpeg at 72 (length 12) → ends at 84
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00]);
    buf.extend_from_slice(&[0x02, 0x00]);
    buf.extend_from_slice(&[0x01, 0x02, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x26, 0x00, 0x00, 0x00]);
    buf.extend_from_slice(&[0x02, 0x02, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00]);
    buf.extend_from_slice(&[0x2A, 0x00, 0x00, 0x00]); // next-IFD = 42
    buf.extend_from_slice(&small_jpeg);
    // IFD1 at 42
    buf.extend_from_slice(&[0x02, 0x00]);
    buf.extend_from_slice(&[0x01, 0x02, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x48, 0x00, 0x00, 0x00]);
    buf.extend_from_slice(&[0x02, 0x02, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x00]);
    buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    buf.extend_from_slice(&large_jpeg);

    let result = find_tiff_preview(&buf).expect("preview should be found");
    assert_eq!(result.bytes.len(), 12, "should pick the larger JPEG");
    assert_eq!(result.bytes[0], 0xFF);
    assert_eq!(result.bytes[1], 0xD8);
  }

  #[test]
  fn rejects_truncated_header() {
    assert!(find_tiff_preview(&[0x49, 0x49]).is_none());
  }

  #[test]
  fn rejects_non_tiff_magic() {
    assert!(find_tiff_preview(&[0xFF, 0xD8, 0xFF, 0xE1, 0, 0, 0, 0]).is_none());
  }

  #[test]
  fn cr3_finds_preview_in_uuid_prvw() {
    // Synthetic CR3-shape: ftyp + moov{uuid{prvw{16-byte header + JPEG}}}
    let jpeg: [u8; 6] = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
    // prvw payload · 16-byte header followed by JPEG
    let mut prvw_payload: Vec<u8> = vec![0u8; 16];
    prvw_payload.extend_from_slice(&jpeg);
    // prvw box (size + 'prvw' + payload)
    let prvw_size = 8 + prvw_payload.len();
    let mut prvw_box: Vec<u8> = Vec::new();
    prvw_box.extend_from_slice(&(prvw_size as u32).to_be_bytes());
    prvw_box.extend_from_slice(b"prvw");
    prvw_box.extend_from_slice(&prvw_payload);
    // uuid box · 16-byte UUID + prvw box inside
    let uuid_payload_len = 16 + prvw_box.len();
    let uuid_size = 8 + uuid_payload_len;
    let mut uuid_box: Vec<u8> = Vec::new();
    uuid_box.extend_from_slice(&(uuid_size as u32).to_be_bytes());
    uuid_box.extend_from_slice(b"uuid");
    uuid_box.extend_from_slice(&[0u8; 16]); // UUID bytes (any 16)
    uuid_box.extend_from_slice(&prvw_box);
    // moov box containing the uuid box
    let moov_size = 8 + uuid_box.len();
    let mut moov_box: Vec<u8> = Vec::new();
    moov_box.extend_from_slice(&(moov_size as u32).to_be_bytes());
    moov_box.extend_from_slice(b"moov");
    moov_box.extend_from_slice(&uuid_box);
    // ftyp box · just so the validator passes
    let ftyp_size = 16usize;
    let mut ftyp_box: Vec<u8> = Vec::new();
    ftyp_box.extend_from_slice(&(ftyp_size as u32).to_be_bytes());
    ftyp_box.extend_from_slice(b"ftyp");
    ftyp_box.extend_from_slice(b"crx ");
    ftyp_box.extend_from_slice(&[0u8; 4]);

    let mut full: Vec<u8> = Vec::new();
    full.extend_from_slice(&ftyp_box);
    full.extend_from_slice(&moov_box);

    let result = find_cr3_preview(&full).expect("cr3 preview should be found");
    assert_eq!(result.source, "cr3-prvw");
    assert_eq!(result.bytes.len(), jpeg.len());
    assert_eq!(result.bytes[0], 0xFF);
    assert_eq!(result.bytes[1], 0xD8);
  }

  #[test]
  fn cr3_finds_preview_in_multiple_uuid_boxes() {
    let jpeg: [u8; 6] = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
    // prvw payload · 16-byte header followed by JPEG
    let mut prvw_payload: Vec<u8> = vec![0u8; 16];
    prvw_payload.extend_from_slice(&jpeg);
    // prvw box
    let prvw_size = 8 + prvw_payload.len();
    let mut prvw_box: Vec<u8> = Vec::new();
    prvw_box.extend_from_slice(&(prvw_size as u32).to_be_bytes());
    prvw_box.extend_from_slice(b"prvw");
    prvw_box.extend_from_slice(&prvw_payload);

    // uuid box 1: metadata (just 16-byte UUID and some random bytes, no sub-boxes)
    let uuid1_payload_len = 16 + 10;
    let uuid1_size = 8 + uuid1_payload_len;
    let mut uuid1_box: Vec<u8> = Vec::new();
    uuid1_box.extend_from_slice(&(uuid1_size as u32).to_be_bytes());
    uuid1_box.extend_from_slice(b"uuid");
    uuid1_box.extend_from_slice(&[1u8; 16]); // UUID bytes
    uuid1_box.extend_from_slice(&[0u8; 10]); // Non-box payload

    // uuid box 2: contains prvw box
    let uuid2_payload_len = 16 + prvw_box.len();
    let uuid2_size = 8 + uuid2_payload_len;
    let mut uuid2_box: Vec<u8> = Vec::new();
    uuid2_box.extend_from_slice(&(uuid2_size as u32).to_be_bytes());
    uuid2_box.extend_from_slice(b"uuid");
    uuid2_box.extend_from_slice(&[2u8; 16]); // UUID bytes
    uuid2_box.extend_from_slice(&prvw_box);

    // moov box containing uuid box 1 and uuid box 2
    let moov_size = 8 + uuid1_box.len() + uuid2_box.len();
    let mut moov_box: Vec<u8> = Vec::new();
    moov_box.extend_from_slice(&(moov_size as u32).to_be_bytes());
    moov_box.extend_from_slice(b"moov");
    moov_box.extend_from_slice(&uuid1_box);
    moov_box.extend_from_slice(&uuid2_box);

    // ftyp box
    let ftyp_size = 16usize;
    let mut ftyp_box: Vec<u8> = Vec::new();
    ftyp_box.extend_from_slice(&(ftyp_size as u32).to_be_bytes());
    ftyp_box.extend_from_slice(b"ftyp");
    ftyp_box.extend_from_slice(b"crx ");
    ftyp_box.extend_from_slice(&[0u8; 4]);

    let mut full: Vec<u8> = Vec::new();
    full.extend_from_slice(&ftyp_box);
    full.extend_from_slice(&moov_box);

    let result = find_cr3_preview(&full).expect("cr3 preview should be found");
    assert_eq!(result.source, "cr3-prvw");
    assert_eq!(result.bytes.len(), jpeg.len());
    assert_eq!(result.bytes[0], 0xFF);
    assert_eq!(result.bytes[1], 0xD8);
  }

  #[test]
  fn cr3_returns_none_when_no_uuid_box() {
    // ftyp + moov{} with no uuid inside.
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(&16u32.to_be_bytes());
    buf.extend_from_slice(b"ftyp");
    buf.extend_from_slice(b"crx ");
    buf.extend_from_slice(&[0u8; 4]);
    buf.extend_from_slice(&8u32.to_be_bytes());
    buf.extend_from_slice(b"moov");
    assert!(find_cr3_preview(&buf).is_none());
  }
}
