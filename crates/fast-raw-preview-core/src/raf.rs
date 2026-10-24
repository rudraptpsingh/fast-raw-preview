//! Fujifilm RAF parser — locates the embedded JPEG and delegates to the
//! JPEG parser for actual EXIF extraction.
//!
//! RAF layout (only the parts we need):
//!
//!   Offset  Size  Field
//!   ------  ----  -----------------------------------------
//!   0x00    16    Magic: "FUJIFILMCCD-RAW "
//!   0x10     4    Format version (ASCII digits)
//!   0x14     8    Camera ID string
//!   0x1C    32    Camera variant
//!   0x3C     4    Directory version
//!   0x40    20    (unused / reserved)
//!   0x54     4    JPEG image offset (big-endian u32)
//!   0x58     4    JPEG image length (big-endian u32)
//!   0x5C    ...   (CFA header offset / length, etc.)
//!
//! The embedded JPEG at offset 0x54..0x58 is a full self-contained JPEG
//! including its APP1/EXIF segment. Once we know the offset we hand off to
//! `jpeg::parse_jpeg`.

use crate::jpeg;
use crate::tags::ParsedTags;

const RAF_MAGIC: &[u8] = b"FUJIFILMCCD-RAW ";

/// Try to parse a RAF file's metadata. Returns `Some(())` when an embedded
/// JPEG with EXIF was found and parsed; `None` when the file isn't a RAF
/// or its embedded JPEG is missing/malformed.
pub fn parse_raf(data: &[u8], out: &mut ParsedTags) -> Option<()> {
  if data.len() < 0x60 {
    return None;
  }
  if &data[0..16] != RAF_MAGIC {
    return None;
  }
  let jpeg_off = u32::from_be_bytes([data[0x54], data[0x55], data[0x56], data[0x57]]) as usize;
  let jpeg_len = u32::from_be_bytes([data[0x58], data[0x59], data[0x5A], data[0x5B]]) as usize;
  // Defensive: cap the embedded JPEG slice to whatever we have on hand.
  if jpeg_off >= data.len() {
    return None;
  }
  let end = (jpeg_off + jpeg_len).min(data.len());
  jpeg::parse_jpeg(&data[jpeg_off..end], out)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rejects_non_raf() {
    let mut tags = ParsedTags::default();
    let buf = vec![0u8; 0x100];
    assert!(parse_raf(&buf, &mut tags).is_none());
  }

  #[test]
  fn rejects_truncated() {
    let mut tags = ParsedTags::default();
    assert!(parse_raf(&[0; 10], &mut tags).is_none());
  }
}
