/// Human-readable byte sizes and counts.
///
/// All formatters take plain numbers so they can be tested without a scan model.

/// Format bytes using binary units (1 KB = 1024 B), which matches how
/// disk analyzers report allocated space.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Format a count with thousands separators: 1423912 -> "1,423,912".
pub fn format_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let len = s.len();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Format a percent share of `total` for `part`, e.g. 17.1% .
pub fn format_percent(part: u64, total: u64) -> String {
    if total == 0 || part == 0 {
        return "0%".to_string();
    }
    let pct = (part as f64 / total as f64) * 100.0;
    if pct >= 99.95 {
        "100%".to_string()
    } else {
        format!("{pct:.1}%")
    }
}

/// Format a unix timestamp (milliseconds) relative to now, for table rows.
pub fn format_modified(unix_ms: Option<i64>, now_unix_ms: i64) -> String {
    use chrono::{DateTime, Local, Utc};

    let Some(ms) = unix_ms else {
        return "-".to_string();
    };
    let Some(dt) = DateTime::<Utc>::from_timestamp_millis(ms) else {
        return "-".to_string();
    };
    let local: DateTime<Local> = dt.into();
    let today = Local::now().date_naive();
    let day = local.date_naive();

    if day == today {
        return format!("Today {}", local.format("%H:%M"));
    }
    if day == today.succ_opt().unwrap_or(today) {
        return "Yesterday".to_string();
    }
    let days_ago = (now_unix_ms - ms) / 86_400_000;
    let current_year = Local::now().format("%Y").to_string();
    let same_year = local.format("%Y").to_string() == current_year;
    if days_ago < 180 && same_year {
        local.format("%b %d").to_string()
    } else {
        local.format("%Y-%m-%d").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(42_101), "41.1 KB");
        assert_eq!(format_size(782 * 1024 * 1024), "782 MB");
        assert_eq!(format_size(14 * 1024 * 1024 * 1024 + 800_000_000), "14.7 GB");
    }

    #[test]
    fn counts() {
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(142_391), "142,391");
    }

    #[test]
    fn percents() {
        assert_eq!(format_percent(0, 100), "0%");
        assert_eq!(format_percent(50, 100), "50.0%");
        assert_eq!(format_percent(171, 1000), "17.1%");
    }
}
