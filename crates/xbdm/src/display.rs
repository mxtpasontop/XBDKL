//! Text formatting shared by the command line, the shell and the app.

use std::time::{SystemTime, UNIX_EPOCH};

/// `1.5 GiB`, `20 B`, ...
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

/// Splits a time into UTC (year, month, day, hour, minute, second).
pub fn civil_time(t: SystemTime) -> (i64, u32, u32, u32, u32, u32) {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil-from-days (Howard Hinnant), valid for dates after 1970.
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, (rem / 3600) as u32, (rem % 3600 / 60) as u32, (rem % 60) as u32)
}

/// `YYYY-MM-DD HH:MM` in UTC, or blanks of the same width when unknown.
pub fn format_time(t: Option<SystemTime>) -> String {
    match t {
        Some(t) if t >= UNIX_EPOCH => {
            let (y, mo, d, h, mi, _) = civil_time(t);
            format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
        }
        _ => " ".repeat(16),
    }
}

/// Classic 16-bytes-per-row hex dump; unmapped bytes show as `??`.
pub fn hexdump(addr: u32, bytes: &[Option<u8>]) -> String {
    let mut out = String::new();
    for (row, chunk) in bytes.chunks(16).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|b| b.map_or("??".into(), |b| format!("{b:02X}"))).collect();
        let ascii: String = chunk
            .iter()
            .map(|b| match b {
                Some(c) if c.is_ascii_graphic() || *c == b' ' => *c as char,
                _ => '.',
            })
            .collect();
        out.push_str(&format!("{:08X}  {:<47}  {ascii}\n", addr.wrapping_add(row as u32 * 16), hex.join(" ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn formatting() {
        assert_eq!(human_size(20), "20 B");
        assert_eq!(human_size(1536), "1.5 KiB");
        let t = UNIX_EPOCH + Duration::from_secs(1_132_660_904);
        assert_eq!(format_time(Some(t)), "2005-11-22 12:01");
        assert_eq!(format_time(None).len(), 16);
        assert!(hexdump(0x10, &[Some(b'A'), None]).starts_with("00000010  41 ??"));
    }
}
