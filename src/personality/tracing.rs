//! Tracing subscriber builder. Splits the friendly path (no timestamp, no
//! target, no level word, glyph-prefixed) from the off path (v0.13's exact
//! `tracing_subscriber::fmt()` default).
//!
//! `RUST_LOG` and an explicit `--log-level` always force off-mode formatting:
//! a user reaching for those wants verbose output, and dropping the target
//! would defeat the purpose.

use std::fmt;

use tracing::{Event, Level, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, format::Writer};
use tracing_subscriber::registry::LookupSpan;

use crate::personality::Mode;

/// Default env filter when friendly mode is on and the user has not asked for
/// anything more verbose. WARN-and-above only; INFO chatter (schema migration,
/// session validation, watch heartbeats) is intended to be replaced by curated
/// personality lines printed via stdout instead.
pub const FRIENDLY_DEFAULT_FILTER: &str = "warn";

/// Custom event formatter for friendly mode.
///
/// Writes a colored glyph prefix derived from the event level, then the
/// message body. No timestamp, no target, no level word, no field set.
///
/// Color is applied via `console::Style`, which strips ANSI sequences when
/// `NO_COLOR` is set or when the writer target is not a terminal. We do not
/// branch on those conditions ourselves.
pub struct FriendlyFormat;

impl<S, N> FormatEvent<S, N> for FriendlyFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let level = *event.metadata().level();
        let (glyph, style) = match level {
            Level::ERROR => ("x ", console::Style::new().red().bold()),
            Level::WARN => ("! ", console::Style::new().yellow()),
            // INFO/DEBUG/TRACE never reach friendly mode under default filter
            // (FRIENDLY_DEFAULT_FILTER is "warn"). Render plainly if a caller
            // raises verbosity without disabling friendly.
            _ => ("", console::Style::new()),
        };
        if !glyph.is_empty() {
            write!(writer, "{}", style.apply_to(glyph))?;
        }
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

/// Build the env filter, preferring `RUST_LOG` if set.
#[must_use]
pub fn env_filter(default_filter: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter))
}

/// Effective default filter for the resolved mode, given the user's chosen
/// log level (which only applies in off mode; friendly forces off if log
/// level is explicit, so this is only consulted when friendly was selected
/// AND the level is the implicit default).
#[must_use]
pub fn default_filter_for(mode: Mode, off_filter: &str) -> String {
    match mode {
        Mode::Friendly => FRIENDLY_DEFAULT_FILTER.to_string(),
        Mode::Off => off_filter.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_mode_filter_passes_through() {
        assert_eq!(default_filter_for(Mode::Off, "kei=info"), "kei=info");
        assert_eq!(
            default_filter_for(Mode::Off, "kei=debug,info"),
            "kei=debug,info"
        );
        assert_eq!(default_filter_for(Mode::Off, "warn"), "warn");
    }

    #[test]
    fn friendly_mode_uses_warn_default() {
        assert_eq!(default_filter_for(Mode::Friendly, "kei=info"), "warn");
        // Even if the off-path would have been verbose, friendly silences INFO.
        assert_eq!(default_filter_for(Mode::Friendly, "kei=debug,info"), "warn");
    }

    #[test]
    fn env_filter_uses_default_when_rust_log_unset() {
        // RUST_LOG is process-global, so we exercise only the default-fallback
        // path here; the override path is tracing-subscriber's contract, not
        // ours, and mutating RUST_LOG from a test races with parallel runs.
        let filter = env_filter("warn");
        let rendered = format!("{filter}");
        assert!(
            rendered.contains("warn") || rendered.is_empty(),
            "expected fallback filter to render warn, got {rendered:?}"
        );
    }

    #[test]
    fn friendly_default_filter_constant_is_warn() {
        assert_eq!(FRIENDLY_DEFAULT_FILTER, "warn");
    }

    /// Capture tracing output to a `Vec<u8>` so we can compare friendly vs.
    /// off rendering byte-for-byte. Same pattern used in
    /// `icloud::photos::album::fetcher_response_body_only_logs_at_trace`.
    fn capture_with<F>(build: F) -> String
    where
        F: FnOnce(std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> tracing::dispatcher::Dispatch,
    {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let dispatch = build(std::sync::Arc::clone(&buf));
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::warn!("first warning event");
            tracing::error!("first error event");
            // INFO lands only when the filter lets it through; under the
            // friendly default ("warn") it should be silently dropped.
            tracing::info!("info event that should be hidden by friendly default");
        });
        let bytes = buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn make_writer_for(
        buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    ) -> impl for<'a> tracing_subscriber::fmt::MakeWriter<'a> + 'static {
        struct VecMakeWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        struct VecWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for VecWriter {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for VecMakeWriter {
            type Writer = VecWriter;
            fn make_writer(&'a self) -> Self::Writer {
                VecWriter(std::sync::Arc::clone(&self.0))
            }
        }
        VecMakeWriter(buf)
    }

    #[test]
    fn off_mode_tracing_includes_target_and_level() {
        let captured = capture_with(|buf| {
            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::new("info"))
                .with_writer(make_writer_for(buf))
                .with_ansi(false)
                .finish()
                .into()
        });
        // v0.13 baseline: target (kei::personality::tracing::tests),
        // level word (INFO/WARN/ERROR), and a timestamp (digits + "Z" or "T").
        assert!(
            captured.contains("WARN"),
            "off mode must include WARN level word; got: {captured}"
        );
        assert!(
            captured.contains("ERROR"),
            "off mode must include ERROR level word; got: {captured}"
        );
        assert!(
            captured.contains("kei::personality::tracing::tests"),
            "off mode must include module target; got: {captured}",
        );
        assert!(
            captured.contains("info event that should be hidden by friendly default"),
            "off mode at filter=info must include INFO events; got: {captured}",
        );
    }

    #[test]
    fn friendly_mode_tracing_drops_target_and_level_word() {
        let captured = capture_with(|buf| {
            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::new(FRIENDLY_DEFAULT_FILTER))
                .with_writer(make_writer_for(buf))
                .with_ansi(false)
                .with_target(false)
                .with_level(false)
                .without_time()
                .event_format(FriendlyFormat)
                .finish()
                .into()
        });
        // Friendly mode: no module target, no level word, no timestamp.
        // Glyph prefix only ("! " for WARN, "x " for ERROR).
        assert!(
            !captured.contains("kei::personality::tracing"),
            "friendly mode must NOT include module target; got: {captured}",
        );
        assert!(
            !captured.contains(" WARN "),
            "friendly mode must NOT include the WARN level word; got: {captured}",
        );
        assert!(
            !captured.contains(" ERROR "),
            "friendly mode must NOT include the ERROR level word; got: {captured}",
        );
        assert!(
            captured.contains("first warning event"),
            "friendly mode must still emit the WARN message body; got: {captured}",
        );
        assert!(
            captured.contains("first error event"),
            "friendly mode must still emit the ERROR message body; got: {captured}",
        );
        assert!(
            !captured.contains("info event that should be hidden by friendly default"),
            "friendly mode at default filter (warn) must drop INFO events; got: {captured}",
        );
    }

    #[test]
    fn friendly_mode_warn_line_uses_bang_prefix() {
        let captured = capture_with(|buf| {
            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::new(FRIENDLY_DEFAULT_FILTER))
                .with_writer(make_writer_for(buf))
                .with_ansi(false)
                .with_target(false)
                .with_level(false)
                .without_time()
                .event_format(FriendlyFormat)
                .finish()
                .into()
        });
        assert!(
            captured.contains("! ") && captured.contains("first warning event"),
            "WARN line must have `! ` glyph and message body; got: {captured:?}",
        );
        assert!(
            captured.contains("x ") && captured.contains("first error event"),
            "ERROR line must have `x ` glyph and message body; got: {captured:?}",
        );
    }
}
