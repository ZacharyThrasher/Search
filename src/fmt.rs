//! Small human-readable formatters shared by the CLI and the engine log.

use std::time::Duration;

pub fn dur(d: Duration) -> String {
    let us = d.as_secs_f64() * 1e6;
    if us < 1000.0 {
        format!("{us:.0} µs")
    } else if us < 1e6 {
        format!("{:.2} ms", us / 1e3)
    } else {
        format!("{:.2} s", us / 1e6)
    }
}

pub fn mib(bytes: usize) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

pub fn commas(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "5 s", "12 min", "3.5 h", "2.1 d" – coarse ages for log lines.
pub fn age(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 90.0 {
        format!("{s:.0} s")
    } else if s < 5400.0 {
        format!("{:.0} min", s / 60.0)
    } else if s < 129_600.0 {
        format!("{:.1} h", s / 3600.0)
    } else {
        format!("{:.1} d", s / 86_400.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(commas(1_234_567), "1,234,567");
        assert_eq!(commas(12), "12");
        assert_eq!(dur(Duration::from_micros(250)), "250 µs");
        assert_eq!(dur(Duration::from_millis(1500)), "1.50 s");
        assert_eq!(age(Duration::from_secs(30)), "30 s");
        assert_eq!(age(Duration::from_secs(7200)), "2.0 h");
    }
}
