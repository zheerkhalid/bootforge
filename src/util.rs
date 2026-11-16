//! Small formatting helpers.

/// "7.5 GB", "512 MB", "0 bytes" (binary units, as Windows shows them).
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[i])
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

/// The size printed on the box: decimal gigabytes rounded to a marketing size
/// ("32 GB" for a 30.9 billion byte stick).
pub fn nominal_size(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb < 0.9 {
        return format!("{:.0} MB", bytes as f64 / 1e6);
    }
    const SIZES: [f64; 16] = [
        1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 120.0, 128.0, 240.0, 256.0, 480.0, 500.0, 512.0,
        1000.0, 2000.0,
    ];
    for s in SIZES {
        if gb <= s * 1.01 && gb >= s * 0.88 {
            return if s >= 1000.0 {
                format!("{:.0} TB", s / 1000.0)
            } else {
                format!("{s:.0} GB")
            };
        }
    }
    format!("{gb:.0} GB")
}

/// "12.3 MB/s".
pub fn human_speed(bytes_per_second: f64) -> String {
    if !bytes_per_second.is_finite() || bytes_per_second <= 0.0 {
        return "—".into();
    }
    format!("{}/s", human_size(bytes_per_second as u64))
}

/// "1 h 05 min", "3 min 20 s", "45 s".
pub fn human_duration(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    if h > 0 {
        format!("{h} h {m:02} min")
    } else if m > 0 {
        format!("{m} min {s:02} s")
    } else {
        format!("{s} s")
    }
}

/// Lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Normalises a pasted checksum for comparison: hex digits only, lower case.
pub fn normalize_hash(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_hexdigit())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(human_size(0), "0 bytes");
        assert_eq!(human_size(1023), "1023 bytes");
        assert_eq!(human_size(1024), "1.00 KB");
        assert_eq!(
            human_size(5 * 1024 * 1024 * 1024 + 300 * 1024 * 1024),
            "5.29 GB"
        );
        assert_eq!(human_size(120 * 1024 * 1024), "120 MB");
        assert_eq!(nominal_size(30_943_995_904), "32 GB");
        assert_eq!(nominal_size(15_500_000_000), "16 GB");
        assert_eq!(nominal_size(1_000_204_886_016), "1 TB");
        assert_eq!(nominal_size(512_000_000), "512 MB");
    }

    #[test]
    fn durations_and_speed() {
        assert_eq!(human_duration(45), "45 s");
        assert_eq!(human_duration(200), "3 min 20 s");
        assert_eq!(human_duration(3900), "1 h 05 min");
        assert_eq!(human_speed(0.0), "—");
        assert_eq!(human_speed(12.5 * 1024.0 * 1024.0), "12.5 MB/s");
    }

    #[test]
    fn hashes() {
        assert_eq!(hex(&[0, 255, 16]), "00ff10");
        assert_eq!(normalize_hash(" AB:cd 12\n"), "abcd12");
    }
}
