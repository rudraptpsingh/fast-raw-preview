//! JPEG APP1/EXIF extractor.
//!
//! A JPEG starts with FF D8 (SOI) and is followed by zero or more APP
//! segments. The EXIF segment is APP1 (FF E1) whose payload begins with the
//! magic ASCII string `Exif\0\0` followed by a TIFF block.
//!
//! We only walk segment markers; we don't decode pixels. The first 64 KB of
//! a typical JPEG always covers the EXIF segment, so callers can pass a
//! truncated buffer to keep I/O cheap.

use crate::tags::ParsedTags;
use crate::tiff;

/// Maximum bytes we read for EXIF parsing. EXIF spec caps the APP1 segment
/// at 64 KB minus the marker overhead, so reading the first 64 KB of any
/// JPEG always covers the full EXIF block.
pub const EXIF_PEEK_BYTES: usize = 65536;

/// Walk a JPEG's APP markers, find APP1/EXIF, parse its TIFF block.
/// Folds tags into `out`. Returns `Some(())` when an EXIF block was found,
/// `None` when no APP1/EXIF was present (image had its EXIF stripped).
pub fn parse_jpeg(data: &[u8], out: &mut ParsedTags) -> Option<()> {
  if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
    return None;
  }
  let mut pos = 2;
  while pos + 4 <= data.len() {
    if data[pos] != 0xFF {
      // Padding or compressed scan — once SOS (FF DA) hits, the EXIF block
      // (if any) was earlier; we can stop.
      return None;
    }
    let marker = data[pos + 1];
    // Standalone markers (no length): SOI 0xD8, EOI 0xD9, RST0..7 0xD0..D7
    if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
      pos += 2;
      continue;
    }
    if pos + 4 > data.len() {
      return None;
    }
    // Big-endian 16-bit segment length (includes the 2 length bytes).
    let seg_len =
      u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
    if seg_len < 2 {
      return None;
    }
    let payload_start = pos + 4;
    let payload_end = pos + 2 + seg_len;
    if payload_end > data.len() {
      return None;
    }
    if marker == 0xE1 {
      // APP1 — could be EXIF or XMP. EXIF starts with "Exif\0\0".
      let payload = &data[payload_start..payload_end];
      if payload.len() >= 6 && &payload[..6] == b"Exif\0\0" {
        return tiff::parse_tiff_block(&payload[6..], out);
      }
    }
    if marker == 0xDA {
      // SOS — start of compressed image data; EXIF can't appear after.
      return None;
    }
    pos = payload_end;
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Hand-built minimal JPEG with one APP1/EXIF segment containing a
  /// TIFF block with one IFD0 entry: Make = "TestCam".
  fn build_minimal_jpeg() -> Vec<u8> {
    // SOI
    let mut out = vec![0xFF, 0xD8];
    // APP1 marker + length placeholder
    out.extend_from_slice(&[0xFF, 0xE1]);
    let len_pos = out.len();
    out.extend_from_slice(&[0, 0]); // placeholder
                                    // EXIF magic
    out.extend_from_slice(b"Exif\0\0");
    // TIFF header (little-endian, magic, IFD0 offset = 8)
    out.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00]);
    // IFD0: 1 entry
    out.extend_from_slice(&[0x01, 0x00]);
    // Tag 0x010F (Make), type 2 (ASCII), count 8, value-offset = inline?
    // "TestCam\0" = 8 bytes — fits inline only if count <= 4. So count > 4
    // means we need an offset. Let's place the string after the IFD.
    // After IFD entry (12 bytes) + next-IFD offset (4 bytes) = 18 bytes
    // from IFD0 start. IFD0 starts at offset 8 within the TIFF block, so
    // the value offset within the TIFF block is 8 + 2 + 12 + 4 = 26.
    out.extend_from_slice(&[0x0F, 0x01]); // tag 0x010F LE
    out.extend_from_slice(&[0x02, 0x00]); // type ASCII
    out.extend_from_slice(&[0x08, 0x00, 0x00, 0x00]); // count = 8
    out.extend_from_slice(&[0x1A, 0x00, 0x00, 0x00]); // value offset = 26 LE
    // next-IFD offset = 0 (none)
    out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    // The actual ASCII string referenced above
    out.extend_from_slice(b"TestCam\0");
    // Patch APP1 length: seg_len = (segment payload size + 2) where payload
    // is everything from "Exif\0\0" to here.
    let seg_payload_end = out.len();
    let seg_len = seg_payload_end - len_pos; // includes the 2 length bytes
    out[len_pos] = (seg_len >> 8) as u8;
    out[len_pos + 1] = (seg_len & 0xFF) as u8;
    // EOI
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
  }

  #[test]
  fn parse_minimal_jpeg_extracts_make() {
    let jpeg = build_minimal_jpeg();
    let mut tags = ParsedTags::default();
    parse_jpeg(&jpeg, &mut tags).expect("should find APP1/EXIF");
    assert_eq!(tags.make.as_deref(), Some("TestCam"));
  }

  #[test]
  fn parse_jpeg_returns_none_for_non_jpeg() {
    let buf = vec![0u8; 100];
    let mut tags = ParsedTags::default();
    assert!(parse_jpeg(&buf, &mut tags).is_none());
  }

  #[test]
  fn parse_jpeg_returns_none_for_truncated() {
    let mut tags = ParsedTags::default();
    assert!(parse_jpeg(&[0xFF, 0xD8], &mut tags).is_none());
  }
}
