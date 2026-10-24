//! TIFF/EXIF IFD walker.
//!
//! All EXIF data — whether it's inside a JPEG APP1 segment, a TIFF/DNG file,
//! or the embedded JPEG of a RAW container — is structured the same way:
//! a header with byte order + IFD offset, then one or more IFDs containing
//! tagged entries.
//!
//! We don't parse pixel data. We only walk tag entries to extract the
//! handful of strings + dates the burst grouper needs.
//!
//! References:
//!   - TIFF 6.0 spec, sections on byte order and IFDs
//!   - EXIF 2.32 spec, tags 0x0132 (DateTime), 0x9003 (DateTimeOriginal),
//!     0x9291 (SubSecTimeOriginal), 0x010F (Make), 0x0110 (Model),
//!     0xA431 (BodySerialNumber)

use crate::tags::ParsedTags;

/// Endianness of the TIFF/EXIF block. Decoded from the 2-byte byte-order mark
/// at the start of the TIFF header (`II` = little-endian, `MM` = big-endian).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endian {
  Little,
  Big,
}

impl Endian {
  fn read_u16(self, b: &[u8]) -> u16 {
    match self {
      Endian::Little => u16::from_le_bytes([b[0], b[1]]),
      Endian::Big => u16::from_be_bytes([b[0], b[1]]),
    }
  }
  fn read_u32(self, b: &[u8]) -> u32 {
    match self {
      Endian::Little => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
      Endian::Big => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
    }
  }
}

// ─── EXIF tag IDs we care about ──────────────────────────────────────────
// Curated to the burst-grouping hot path. Adding a tag = bump the parser
// surface deliberately, not by accident.
pub const TAG_MAKE: u16 = 0x010F;
pub const TAG_MODEL: u16 = 0x0110;
pub const TAG_DATE_TIME_ORIGINAL: u16 = 0x9003;
pub const TAG_SUB_SEC_TIME_ORIGINAL: u16 = 0x9291;
pub const TAG_OFFSET_TIME_ORIGINAL: u16 = 0x9011;
pub const TAG_BODY_SERIAL_NUMBER: u16 = 0xA431;
/// Camera-specific MakerNote-style serials. Many cameras (Fuji, Olympus)
/// don't write 0xA431 and instead stash a serial under their MakerNote.
/// We don't walk MakerNotes, but Sub-IFDs sometimes carry the same field
/// at the legacy `CameraSerialNumber` position 0xC62F (DNG).
pub const TAG_CAMERA_SERIAL_NUMBER_DNG: u16 = 0xC62F;
/// EXIF SubIFD pointer (tag 0x8769) — Make/Model live in IFD0 but
/// DateTimeOriginal lives in the EXIF SubIFD. We follow the pointer.
pub const TAG_EXIF_IFD_POINTER: u16 = 0x8769;

/// EXIF entry types we understand (ignore the rest).
const TYPE_BYTE: u16 = 1;
const TYPE_ASCII: u16 = 2;
const TYPE_SHORT: u16 = 3;
const TYPE_LONG: u16 = 4;
const TYPE_UNDEFINED: u16 = 7;

/// Parse a TIFF block (header + IFDs) and fold every tag we recognise into
/// `out`. `data` must point at the TIFF byte-order mark (II / MM).
///
/// Returns `Some(())` on success, `None` when the header is malformed.
/// We never panic on bad input — corrupted EXIF should produce empty
/// metadata, not a crash.
pub fn parse_tiff_block(data: &[u8], out: &mut ParsedTags) -> Option<()> {
  if data.len() < 8 {
    return None;
  }
  let endian = match &data[0..2] {
    [0x49, 0x49] => Endian::Little,
    [0x4D, 0x4D] => Endian::Big,
    _ => return None,
  };
  let magic = endian.read_u16(&data[2..4]);
  if magic != 0x002A {
    return None;
  }
  let ifd0_off = endian.read_u32(&data[4..8]) as usize;
  walk_ifd(data, ifd0_off, endian, out, /* depth */ 0)
}

/// Walk one IFD, recursing into the EXIF SubIFD when we see its pointer.
/// Depth-limited at 3 to defend against pathological inputs.
fn walk_ifd(
  data: &[u8],
  off: usize,
  endian: Endian,
  out: &mut ParsedTags,
  depth: u8,
) -> Option<()> {
  if depth > 3 {
    return Some(());
  }
  if off + 2 > data.len() {
    return None;
  }
  let n_entries = endian.read_u16(&data[off..off + 2]) as usize;
  let entries_start = off + 2;
  let entry_size = 12usize;
  if entries_start + n_entries * entry_size > data.len() {
    return None;
  }

  for i in 0..n_entries {
    let p = entries_start + i * entry_size;
    let tag = endian.read_u16(&data[p..p + 2]);
    let dtype = endian.read_u16(&data[p + 2..p + 4]);
    let count = endian.read_u32(&data[p + 4..p + 8]) as usize;
    let value_or_off = &data[p + 8..p + 12];

    match tag {
      TAG_EXIF_IFD_POINTER => {
        let sub_off = endian.read_u32(value_or_off) as usize;
        // Recurse into the EXIF SubIFD.
        let _ = walk_ifd(data, sub_off, endian, out, depth + 1);
      }
      TAG_MAKE => out.make = read_ascii(data, dtype, count, value_or_off, endian),
      TAG_MODEL => out.model = read_ascii(data, dtype, count, value_or_off, endian),
      TAG_BODY_SERIAL_NUMBER | TAG_CAMERA_SERIAL_NUMBER_DNG => {
        // Prefer first non-empty hit — many cameras only write one or the
        // other.
        if out.serial.is_none() {
          out.serial = read_ascii(data, dtype, count, value_or_off, endian);
        }
      }
      TAG_DATE_TIME_ORIGINAL => {
        out.date_time_original = read_ascii(data, dtype, count, value_or_off, endian);
      }
      TAG_SUB_SEC_TIME_ORIGINAL => {
        out.sub_sec_time_original = read_ascii(data, dtype, count, value_or_off, endian);
      }
      TAG_OFFSET_TIME_ORIGINAL => {
        out.offset_time_original = read_ascii(data, dtype, count, value_or_off, endian);
      }
      _ => { /* not interesting */ }
    }
  }
  Some(())
}

/// Read an ASCII or BYTE-typed entry as a String. Returns None when the
/// type is wrong, the data is out of range, or the result is empty after
/// trimming the trailing NUL EXIF requires.
fn read_ascii(
  data: &[u8],
  dtype: u16,
  count: usize,
  value_or_off: &[u8],
  endian: Endian,
) -> Option<String> {
  if dtype != TYPE_ASCII && dtype != TYPE_BYTE && dtype != TYPE_UNDEFINED {
    // Some cameras misuse SHORT/LONG for numeric serials. We accept those
    // by stringifying the value — better than dropping the camera ID.
    if (dtype == TYPE_SHORT || dtype == TYPE_LONG) && count == 1 {
      let n = if dtype == TYPE_SHORT {
        endian.read_u16(value_or_off) as u32
      } else {
        endian.read_u32(value_or_off)
      };
      return Some(n.to_string());
    }
    return None;
  }
  let bytes: &[u8] = if count <= 4 {
    // value fits inline in the 4 reserved bytes
    &value_or_off[..count.min(4)]
  } else {
    let off = endian.read_u32(value_or_off) as usize;
    if off + count > data.len() {
      return None;
    }
    &data[off..off + count]
  };
  // Trim trailing NULs and surrounding whitespace.
  let trimmed = bytes
    .iter()
    .rposition(|&b| b != 0)
    .map(|i| &bytes[..=i])
    .unwrap_or(&[]);
  let s = std::str::from_utf8(trimmed).ok()?.trim();
  if s.is_empty() {
    None
  } else {
    Some(s.to_owned())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  // ─── TIFF-block builders ──────────────────────────────────────────────
  // Real RAW formats (Sony ARW, Nikon NEF, Olympus ORF, Panasonic RW2,
  // Canon CR2, Adobe DNG) are all TIFF containers. They differ in
  // pixel-format details we don't care about; the EXIF tags live in
  // IFD0 (Make/Model) plus the EXIF SubIFD (DateTimeOriginal,
  // SubSec, OffsetTime). The parser handles those generically — these
  // tests build a minimal IFD0 + SubIFD layout per camera brand and
  // assert extraction works without touching the filesystem.

  /// Build a little-endian TIFF block:
  ///   [II*\0][IFD0 offset = 8][IFD0 count][...IFD0 entries...]
  ///
  /// IFD0 entries are written as a contiguous run; trailing-data is
  /// appended after the IFD's next-IFD pointer. The EXIF SubIFD lives
  /// at a fixed offset right after IFD0 so the SubIFD-pointer entry in
  /// IFD0 (tag 0x8769) can find it.
  fn build_tiff_with_exif(
    make: &str,
    model: &str,
    serial: Option<&str>,
    dto: &str,
    sub_sec: Option<&str>,
  ) -> Vec<u8> {
    // We'll stage IFD0 + ExifSubIFD + their string-pool data and
    // resolve byte-offsets at the end.
    let mut buf = Vec::with_capacity(512);
    // 0x00..0x08 — TIFF header.
    buf.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00]); // II + magic 0x002A
    buf.extend_from_slice(&[0x08, 0x00, 0x00, 0x00]); // IFD0 at offset 8

    // ── IFD0 ──
    // entries: Make, Model, optional Serial, EXIF SubIFD pointer (tag 0x8769)
    let ifd0_entries: usize = if serial.is_some() { 4 } else { 3 };
    let ifd0_offset = 8usize;
    let ifd0_size = 2 + ifd0_entries * 12 + 4; // count + entries + next-ifd ptr
    let exif_subifd_offset = ifd0_offset + ifd0_size;
    // SubIFD entries: DateTimeOriginal, optional SubSec
    let sub_entries: usize = if sub_sec.is_some() { 2 } else { 1 };
    let sub_size = 2 + sub_entries * 12 + 4;
    let mut string_pool_offset = exif_subifd_offset + sub_size;

    // String-pool slots
    let make_off = string_pool_offset;
    let make_bytes = c_str(make);
    string_pool_offset += make_bytes.len();
    let model_off = string_pool_offset;
    let model_bytes = c_str(model);
    string_pool_offset += model_bytes.len();
    let serial_off = serial.map(|s| {
      let off = string_pool_offset;
      string_pool_offset += c_str(s).len();
      off
    });
    let dto_off = string_pool_offset;
    let dto_bytes = c_str(dto);
    string_pool_offset += dto_bytes.len();
    let sub_off = sub_sec.map(|s| {
      let off = string_pool_offset;
      string_pool_offset += c_str(s).len();
      off
    });

    // Write IFD0 entry count.
    buf.extend_from_slice(&u16_le(ifd0_entries as u16));
    write_ascii_entry(&mut buf, TAG_MAKE, &make_bytes, make_off);
    write_ascii_entry(&mut buf, TAG_MODEL, &model_bytes, model_off);
    if let Some(s) = serial {
      let serial_bytes = c_str(s);
      write_ascii_entry(
        &mut buf,
        TAG_BODY_SERIAL_NUMBER,
        &serial_bytes,
        serial_off.unwrap(),
      );
    }
    // EXIF SubIFD pointer (tag 0x8769, type LONG=4, count=1, value=offset)
    buf.extend_from_slice(&u16_le(TAG_EXIF_IFD_POINTER));
    buf.extend_from_slice(&u16_le(4)); // LONG
    buf.extend_from_slice(&u32_le(1));
    buf.extend_from_slice(&u32_le(exif_subifd_offset as u32));
    // IFD0 next-ifd pointer = 0 (none).
    buf.extend_from_slice(&u32_le(0));

    // ── EXIF SubIFD ──
    buf.extend_from_slice(&u16_le(sub_entries as u16));
    write_ascii_entry(&mut buf, TAG_DATE_TIME_ORIGINAL, &dto_bytes, dto_off);
    if let Some(s) = sub_sec {
      let sub_bytes = c_str(s);
      write_ascii_entry(
        &mut buf,
        TAG_SUB_SEC_TIME_ORIGINAL,
        &sub_bytes,
        sub_off.unwrap(),
      );
    }
    buf.extend_from_slice(&u32_le(0));

    // ── String pool ──
    buf.extend_from_slice(&make_bytes);
    buf.extend_from_slice(&model_bytes);
    if let Some(s) = serial {
      buf.extend_from_slice(&c_str(s));
    }
    buf.extend_from_slice(&dto_bytes);
    if let Some(s) = sub_sec {
      buf.extend_from_slice(&c_str(s));
    }

    buf
  }

  fn c_str(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
  }
  fn u16_le(n: u16) -> [u8; 2] {
    n.to_le_bytes()
  }
  fn u32_le(n: u32) -> [u8; 4] {
    n.to_le_bytes()
  }
  fn write_ascii_entry(buf: &mut Vec<u8>, tag: u16, bytes: &[u8], pool_offset: usize) {
    let count = bytes.len();
    buf.extend_from_slice(&u16_le(tag));
    buf.extend_from_slice(&u16_le(2)); // ASCII type
    buf.extend_from_slice(&u32_le(count as u32));
    if count <= 4 {
      // EXIF spec: value is inlined into the 4-byte value-or-offset
      // slot, zero-padded if shorter.
      let mut inline = [0u8; 4];
      inline[..count].copy_from_slice(bytes);
      buf.extend_from_slice(&inline);
    } else {
      buf.extend_from_slice(&u32_le(pool_offset as u32));
    }
  }

  fn parse_and_check(
    bytes: Vec<u8>,
    expected_make: &str,
    expected_model: &str,
    expected_serial: Option<&str>,
    expected_dto: &str,
  ) {
    let mut t = ParsedTags::default();
    parse_tiff_block(&bytes, &mut t).expect("TIFF parse should succeed");
    assert_eq!(t.make.as_deref(), Some(expected_make));
    assert_eq!(t.model.as_deref(), Some(expected_model));
    assert_eq!(t.serial.as_deref(), expected_serial);
    assert_eq!(t.date_time_original.as_deref(), Some(expected_dto));
  }

  // ─── Per-format coverage proofs ────────────────────────────────────────

  #[test]
  fn parses_sony_arw_shape() {
    let buf = build_tiff_with_exif(
      "SONY",
      "ILCE-7M4",
      Some("0123456"),
      "2026:02:16 17:59:01",
      Some("123"),
    );
    parse_and_check(buf, "SONY", "ILCE-7M4", Some("0123456"), "2026:02:16 17:59:01");
  }

  #[test]
  fn parses_nikon_nef_shape() {
    let buf = build_tiff_with_exif(
      "NIKON CORPORATION",
      "NIKON Z 9",
      Some("3001234"),
      "2025:08:01 12:00:00",
      None,
    );
    parse_and_check(
      buf,
      "NIKON CORPORATION",
      "NIKON Z 9",
      Some("3001234"),
      "2025:08:01 12:00:00",
    );
  }

  #[test]
  fn parses_canon_cr2_shape() {
    // Canon CR2 (the legacy TIFF-based RAW; CR3 is ISOBMFF and
    // doesn't go through this path).
    let buf = build_tiff_with_exif(
      "Canon",
      "Canon EOS R5",
      Some("123456789"),
      "2025:12:25 09:30:15",
      Some("500"),
    );
    parse_and_check(
      buf,
      "Canon",
      "Canon EOS R5",
      Some("123456789"),
      "2025:12:25 09:30:15",
    );
  }

  #[test]
  fn parses_olympus_orf_shape() {
    let buf = build_tiff_with_exif(
      "OM Digital Solutions",
      "OM-1",
      None, // some Olympus bodies don't write 0xA431
      "2024:11:11 11:11:11",
      None,
    );
    parse_and_check(buf, "OM Digital Solutions", "OM-1", None, "2024:11:11 11:11:11");
  }

  #[test]
  fn parses_panasonic_rw2_shape() {
    let buf = build_tiff_with_exif(
      "Panasonic",
      "DC-S5M2",
      Some("WX12345"),
      "2025:06:15 14:22:33",
      None,
    );
    parse_and_check(
      buf,
      "Panasonic",
      "DC-S5M2",
      Some("WX12345"),
      "2025:06:15 14:22:33",
    );
  }

  #[test]
  fn parses_adobe_dng_shape() {
    let buf = build_tiff_with_exif(
      "DJI",
      "FC8482",
      Some("DRONE-001"),
      "2025:07:04 16:45:00",
      None,
    );
    parse_and_check(buf, "DJI", "FC8482", Some("DRONE-001"), "2025:07:04 16:45:00");
  }

  #[test]
  fn handles_minimum_viable_tiff() {
    // 8-byte header + empty IFD0 with no entries. Should parse without
    // crashing; ParsedTags stays empty.
    let mut buf = vec![0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00];
    buf.extend_from_slice(&[0x00, 0x00]); // n_entries = 0
    buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // next-IFD = 0
    let mut tags = ParsedTags::default();
    parse_tiff_block(&buf, &mut tags).expect("empty IFD0 should still parse");
    assert!(tags.make.is_none());
    assert!(tags.date_time_original.is_none());
  }

  #[test]
  fn rejects_non_tiff_magic() {
    let mut tags = ParsedTags::default();
    let buf = vec![0xFF, 0xD8, 0xFF, 0xE1, 0, 0, 0, 0];
    assert!(parse_tiff_block(&buf, &mut tags).is_none());
  }

  #[test]
  fn rejects_truncated_header() {
    let mut tags = ParsedTags::default();
    assert!(parse_tiff_block(&[0x49, 0x49], &mut tags).is_none());
  }
}
