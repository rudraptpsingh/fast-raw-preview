//! Folded representation of the parsed-out tags. Each parser fills this in;
//! the napi layer reads it to build the output `BurstMeta` row.

#[derive(Default, Debug, Clone)]
pub struct ParsedTags {
  pub make: Option<String>,
  pub model: Option<String>,
  pub serial: Option<String>,
  /// EXIF format `YYYY:MM:DD HH:MM:SS` ASCII.
  pub date_time_original: Option<String>,
  /// EXIF format integer string `123` (millisecond fragment).
  pub sub_sec_time_original: Option<String>,
  /// EXIF format `+05:30` or `Z` ASCII.
  pub offset_time_original: Option<String>,
}

impl ParsedTags {
  /// Compose `Make|Model|Serial`. Empties / Nones drop out so the result is
  /// `Make|Model` when no serial, just `Make` if no model. Returns None when
  /// nothing useful is present.
  pub fn camera_id(&self) -> Option<String> {
    let parts: Vec<&str> = [&self.make, &self.model, &self.serial]
      .iter()
      .filter_map(|o| o.as_deref())
      .map(|s| s.trim())
      .filter(|s| !s.is_empty())
      .collect();
    if parts.is_empty() {
      None
    } else {
      Some(parts.join("|"))
    }
  }

  /// Convert the captured ASCII timestamp + sub-second + offset into ms since
  /// epoch. Returns None on missing/malformed data.
  ///
  /// EXIF DateTimeOriginal is "YYYY:MM:DD HH:MM:SS" (note colons even in
  /// the date). We treat absent OffsetTimeOriginal as local time (matches
  /// what cameras intend — most don't write timezone). The rendered ms is
  /// good enough for burst-gap comparisons (users don't move time zones
  /// between two consecutive shutter clicks).
  pub fn date_time_original_ms(&self) -> Option<f64> {
    let dto = self.date_time_original.as_deref()?;
    // Strict: 19 chars, colon-separated.
    if dto.len() < 19 {
      return None;
    }
    let bytes = dto.as_bytes();
    let parse = |s: &[u8]| -> Option<i64> {
      let st = std::str::from_utf8(s).ok()?;
      st.parse::<i64>().ok()
    };
    let y = parse(&bytes[0..4])?;
    let mo = parse(&bytes[5..7])?;
    let d = parse(&bytes[8..10])?;
    let h = parse(&bytes[11..13])?;
    let mi = parse(&bytes[14..16])?;
    let se = parse(&bytes[17..19])?;
    if !(1970..=2999).contains(&y)
      || !(1..=12).contains(&mo)
      || !(1..=31).contains(&d)
      || !(0..=23).contains(&h)
      || !(0..=59).contains(&mi)
      || !(0..=60).contains(&se)
    {
      return None;
    }
    // Compute days-since-epoch using the proleptic Gregorian calendar.
    // Accurate enough for capture timestamps; we don't need leap-second
    // precision for burst grouping.
    let days = days_since_epoch(y as i32, mo as u32, d as u32)?;
    let mut ms: i64 = days as i64 * 86_400_000 + h * 3_600_000 + mi * 60_000 + se * 1_000;
    // Sub-second fraction is ALWAYS milliseconds (cameras write 1-3 digits).
    if let Some(sub) = self.sub_sec_time_original.as_deref() {
      if let Ok(n) = sub.trim().parse::<u32>() {
        // EXIF SubSecTime is the fractional part of seconds, with the number
        // of digits implying the precision (1=tenths, 2=hundredths, 3=ms).
        let digits = sub.trim().len() as u32;
        let scale = 10u32.checked_pow(3u32.saturating_sub(digits)).unwrap_or(1);
        ms += (n * scale) as i64;
      }
    }
    // OffsetTimeOriginal: "+HH:MM" or "-HH:MM" or "Z". Cameras rarely write
    // it; when present, subtract the offset to land on UTC ms.
    if let Some(off) = self.offset_time_original.as_deref() {
      if let Some(off_ms) = parse_offset(off.trim()) {
        ms -= off_ms;
      }
    }
    Some(ms as f64)
  }
}

/// Days since 1970-01-01 (UTC) — Howard Hinnant's `days_from_civil`.
/// Accepts any year ≥ 0.
fn days_since_epoch(y: i32, m: u32, d: u32) -> Option<i64> {
  if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
    return None;
  }
  let y = if m <= 2 { y - 1 } else { y };
  let era = y.div_euclid(400);
  let yoe = (y - era * 400) as u32;
  let doy = ((153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1) as u32;
  let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
  Some(era as i64 * 146097 + doe as i64 - 719468)
}

fn parse_offset(s: &str) -> Option<i64> {
  if s.eq_ignore_ascii_case("Z") {
    return Some(0);
  }
  if s.len() < 6 {
    return None;
  }
  let sign = match s.as_bytes()[0] {
    b'+' => 1,
    b'-' => -1,
    _ => return None,
  };
  let hh: i64 = s[1..3].parse().ok()?;
  let mm: i64 = s[4..6].parse().ok()?;
  Some(sign * (hh * 3_600_000 + mm * 60_000))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn camera_id_joins_filtered_parts() {
    let t = ParsedTags {
      make: Some("FUJIFILM".into()),
      model: Some("X-T5".into()),
      serial: Some("5CA63863".into()),
      ..Default::default()
    };
    assert_eq!(t.camera_id().as_deref(), Some("FUJIFILM|X-T5|5CA63863"));
  }

  #[test]
  fn camera_id_drops_empty() {
    let t = ParsedTags {
      make: Some("Canon".into()),
      model: None,
      serial: Some("".into()),
      ..Default::default()
    };
    assert_eq!(t.camera_id().as_deref(), Some("Canon"));
  }

  #[test]
  fn camera_id_returns_none_when_empty() {
    let t = ParsedTags::default();
    assert!(t.camera_id().is_none());
  }

  // Reference values cross-checked with `new Date(...).getTime()` in V8.

  #[test]
  fn dto_to_ms_basic() {
    let t = ParsedTags {
      date_time_original: Some("2026:02:16 17:59:01".into()),
      ..Default::default()
    };
    assert_eq!(t.date_time_original_ms().unwrap() as i64, 1_771_264_741_000);
  }

  #[test]
  fn dto_to_ms_with_sub_sec_3_digits() {
    let t = ParsedTags {
      date_time_original: Some("2026:02:16 17:59:01".into()),
      sub_sec_time_original: Some("123".into()),
      ..Default::default()
    };
    assert_eq!(t.date_time_original_ms().unwrap() as i64, 1_771_264_741_123);
  }

  #[test]
  fn dto_to_ms_with_sub_sec_1_digit() {
    let t = ParsedTags {
      date_time_original: Some("2026:02:16 17:59:01".into()),
      sub_sec_time_original: Some("5".into()), // = 0.5s = 500ms
      ..Default::default()
    };
    assert_eq!(t.date_time_original_ms().unwrap() as i64, 1_771_264_741_500);
  }

  #[test]
  fn dto_to_ms_with_offset() {
    // 2026-02-16T17:59:01+05:30 ≡ 2026-02-16T12:29:01Z
    let t = ParsedTags {
      date_time_original: Some("2026:02:16 17:59:01".into()),
      offset_time_original: Some("+05:30".into()),
      ..Default::default()
    };
    assert_eq!(t.date_time_original_ms().unwrap() as i64, 1_771_244_941_000);
  }

  #[test]
  fn dto_to_ms_rejects_garbage() {
    let t = ParsedTags {
      date_time_original: Some("never:gonna:give 17:59:01".into()),
      ..Default::default()
    };
    assert!(t.date_time_original_ms().is_none());
  }
}
