use std::time::{Duration, SystemTime};

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

pub fn age_days(modified: Option<SystemTime>) -> Option<u64> {
    let elapsed = SystemTime::now()
        .duration_since(modified?)
        .unwrap_or(Duration::ZERO);
    Some(elapsed.as_secs() / 86_400)
}

pub fn format_age(modified: Option<SystemTime>) -> String {
    match age_days(modified) {
        None => "?".into(),
        Some(0) => "hoy".into(),
        Some(d @ 1..=30) => format!("{d}d"),
        Some(d @ 31..=364) => format!("{}m", d / 30),
        Some(d) => format!("{:.1}a", d as f64 / 365.0),
    }
}

/// Recorta por la izquierda para que quepa en `width` caracteres: "…/proyecto/node_modules".
pub fn truncate_left(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let skip = len - (width - 1);
    std::iter::once('…').chain(s.chars().skip(skip)).collect()
}

pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.50 KB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.00 GB");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate_left("abcdef", 10), "abcdef");
        assert_eq!(truncate_left("abcdef", 4), "…def");
    }

    #[test]
    fn escaping() {
        assert_eq!(json_escape(r#"C:\a"b"#), r#"C:\\a\"b"#);
    }
}
