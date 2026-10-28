//! Human-readable sizes and rates with fixed widths, so columns do not shift between refreshes.

/// Bytes as a 1024-based figure with one unit letter, at most 6 characters ("1023B", "1.5K", "12.3M", "1.02G").
/// `None` renders blank: a value that does not exist is never shown as zero.
pub fn bytes(v: Option<u64>) -> String {
    match v {
        None => String::new(),
        Some(v) => scaled(v as f64, ""),
    }
}

/// A signed per-second rate: "+1.2M/s", "-12K/s", "0/s" for a measured zero, blank for unknown.
pub fn rate(v: Option<i64>) -> String {
    match v {
        None => String::new(),
        Some(0) => "0/s".to_string(),
        Some(v) if v > 0 => format!("+{}/s", scaled(v as f64, "")),
        Some(v) => format!("-{}/s", scaled((-v) as f64, "")),
    }
}

fn scaled(mut v: f64, suffix: &str) -> String {
    const UNITS: [&str; 7] = ["B", "K", "M", "G", "T", "P", "E"];
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    let s = if u == 0 {
        format!("{}B", v as u64)
    } else if v < 10.0 {
        format!("{:.2}{}", v, UNITS[u])
    } else if v < 100.0 {
        format!("{:.1}{}", v, UNITS[u])
    } else {
        format!("{:.0}{}", v, UNITS[u])
    };
    format!("{}{}", s, suffix)
}

/// Whole-number percent of `part` in `whole`, from integers; None when `whole` is zero.
pub fn percent(part: u64, whole: u64) -> Option<u64> {
    if whole == 0 {
        None
    } else {
        Some((part as u128 * 100 / whole as u128) as u64)
    }
}

/// Wall-clock milliseconds since the epoch as local "HH:MM:SS".
pub fn clock(wall_ms: u64) -> String {
    let secs = (wall_ms / 1000) as i64;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    #[allow(deprecated)]
    let t = secs as libc::time_t;
    unsafe {
        libc::localtime_r(&t, &mut tm);
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(bytes(None), "");
        assert_eq!(bytes(Some(0)), "0B");
        assert_eq!(bytes(Some(1023)), "1023B");
        assert_eq!(bytes(Some(1536)), "1.50K");
        assert_eq!(bytes(Some(12 << 20)), "12.0M");
        assert_eq!(bytes(Some(1_100_000_000)), "1.02G");
        assert_eq!(bytes(Some(900 << 30)), "900G");
    }

    #[test]
    fn rates() {
        assert_eq!(rate(None), "");
        assert_eq!(rate(Some(0)), "0/s");
        assert_eq!(rate(Some(2048)), "+2.00K/s");
        assert_eq!(rate(Some(-3 << 20)), "-3.00M/s");
    }

    #[test]
    fn percents_are_integer() {
        assert_eq!(percent(1, 3), Some(33));
        assert_eq!(percent(0, 0), None);
        assert_eq!(percent(u64::MAX, u64::MAX), Some(100));
    }
}
