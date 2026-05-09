//! Shared formatting helpers for friendly-mode output.
//!
//! Lives in `personality/` because every consumer is a friendly-mode
//! renderer; off-mode logging keeps the pipeline's own
//! `download::pipeline::format_bytes` (always-decimal `GiB`) so journals
//! stay byte-identical to v0.13.

/// Format byte counts for friendly-mode renderers. Integer at >= 10 of a
/// unit, one decimal below 10, so 412 GB and 8.4 GB read distinctly
/// without 412.0 GB looking awkward. 1024-based units displayed as `GB`
/// to match the UX mock.
#[allow(
    clippy::cast_precision_loss,
    reason = "display-only byte formatting; precision loss at exabyte scale is fine"
)]
pub(crate) fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1_024;
    const MB: u64 = KB * 1_024;
    const GB: u64 = MB * 1_024;
    const TB: u64 = GB * 1_024;
    if bytes >= 10 * TB {
        format!("{} TB", bytes / TB)
    } else if bytes >= TB {
        format!("{:.1} TB", bytes as f64 / TB as f64)
    } else if bytes >= 10 * GB {
        format!("{} GB", bytes / GB)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_uses_integer_at_or_above_ten_units() {
        assert_eq!(format_bytes(412_u64 * 1024 * 1024 * 1024), "412 GB");
        assert_eq!(
            format_bytes(8_u64 * 1024 * 1024 * 1024 + 400 * 1024 * 1024),
            "8.4 GB"
        );
        assert_eq!(format_bytes(500 * 1024 * 1024), "500 MB");
        assert_eq!(format_bytes(1024), "1 KB");
        assert_eq!(format_bytes(0), "0 B");
    }
}
