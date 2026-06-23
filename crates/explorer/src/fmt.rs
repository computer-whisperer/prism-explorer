//! Column formatting.

use std::time::SystemTime;

use chrono::{DateTime, Local};

/// Decimal units, matching what file managers show.
pub fn human_bytes(b: u64) -> String {
    let b = b as f64;
    if b >= 1e15 {
        format!("{:.2} PB", b / 1e15)
    } else if b >= 1e12 {
        format!("{:.2} TB", b / 1e12)
    } else if b >= 1e9 {
        format!("{:.2} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} kB", b / 1e3)
    } else {
        format!("{b:.0} B")
    }
}

/// A count with thousands separators (`4623129` → `4,623,129`), for the
/// recursive file/subdir totals that can run into the millions.
pub fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn mtime(t: SystemTime) -> String {
    DateTime::<Local>::from(t)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_round_like_a_file_manager() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(24_300_000), "24.3 MB");
        assert_eq!(human_bytes(3_456_789_012), "3.46 GB");
        assert_eq!(human_bytes(50_850_020_200_770), "50.85 TB");
    }

    #[test]
    fn counts_group_in_threes() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(42), "42");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(4_623_129), "4,623,129");
    }
}
