//! What GLib said before it took the process, in the log rather than only in journald.
//!
//! **GDK aborts from `g_error` when the Wayland connection turns invalid**, and
//! `g_error` does not unwind. So nothing `orchd::logging::install_panic_hook`
//! installs ever runs, and `orchd.log` ends mid-line with no record that anything
//! went wrong. That is the same silence `appkit_abort` covers on macOS, reached by
//! a different mechanism.
//!
//! This app has died that way twice, on 2026-09-09 and 2026-09-21, both times as
//! the same pair:
//!
//! ```text
//! gdk_wayland_window_get_wl_surface: assertion 'GDK_IS_WAYLAND_WINDOW (window)' failed
//! Error 22 (Invalid argument) dispatching to Wayland display.
//! ```
//!
//! **The caller is WebKitGTK, and finding that out took `nm`**: `libwebkit2gtk-4.1`
//! is the only library in the process with an undefined reference to that symbol,
//! and `libgtk-3` has none, so the app's own window code is not the one asking.
//! WebKit asks for the `wl_surface` of a window GDK will not accept, gets NULL, and
//! sends NULL as a non-nullable Wayland argument. That is the `EINVAL` on the
//! second line. The second death took GNOME Shell with it, because an extension
//! then tiled a window that had stopped existing.
//!
//! Neither line reached `orchd.log`. Both went to journald, which is not the file
//! this app tells people to send, and which only exists on one of the three
//! platforms it ships to.
//!
//! The writer cannot prevent the abort and does not try. It writes the message and
//! a backtrace, and the process goes down exactly as it would have. **The backtrace
//! is the half that names the caller**, which is the question the last one could
//! not answer from the message alone. It symbolises C frames, so a WebKit fault
//! arrives with WebKit's frames on it.
//!
//! Two properties worth knowing. **Chained rather than replaced**: every message
//! goes on to `log_writer_default` afterwards, so journald and a terminal keep what
//! they had, and a diagnostic that silences another diagnostic is a poor trade. And
//! only `Warning` and worse are copied, because GTK is talkative at `Message` and
//! below, and a log nobody will read is the failure being fixed.

use gtk::glib::{self, LogField, LogLevel};

/// Install the writer. Call once, early, after the logger exists and before GTK
/// starts: `log_set_writer_func` panics on a second call, and a message logged
/// before it is installed is one this never sees.
pub(crate) fn log_glib_messages() {
    glib::log_set_writer_func(|level, fields| {
        let (domain, message) = domain_and_message(fields);
        match level {
            // Always fatal: GLib aborts once the writer returns.
            LogLevel::Error => tracing::error!(
                "{domain} is taking the process down: {message}\n{}",
                std::backtrace::Backtrace::force_capture()
            ),
            /* Not fatal by itself, and worth the stack anyway: both deaths so far
            opened with a critical from a caller the message did not name, and the
            fatal line a moment later named it no better. */
            LogLevel::Critical => tracing::error!(
                "{domain} raised a critical: {message}\n{}",
                std::backtrace::Backtrace::force_capture()
            ),
            LogLevel::Warning => tracing::warn!("{domain}: {message}"),
            _ => {}
        }
        glib::log_writer_default(level, fields)
    });
    tracing::debug!("GLib faults will now name themselves in the log");
}

/// The two fields worth reading out of a structured GLib message.
///
/// Both are absent often enough to matter. A `g_log` call with no domain set
/// carries no `GLIB_DOMAIN`, and a field written as raw bytes is not UTF-8, and an
/// empty string in the line is better than no line at all.
fn domain_and_message<'f>(fields: &'f [LogField<'_>]) -> (&'f str, &'f str) {
    let field = |key: &str| {
        fields
            .iter()
            .find(|f| f.key() == key)
            .and_then(|f| f.value_str())
            .unwrap_or_default()
    };
    (field("GLIB_DOMAIN"), field("MESSAGE"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::glib::gstr;

    /// The fields GLib actually sends, read back in either order.
    ///
    /// Order is not promised by `g_log_structured`, and reading by position is the
    /// bug this pins: it would put the message in the domain's place and log a
    /// sentence that reads backwards on the one line anybody will see.
    #[test]
    fn a_structured_message_is_read_by_key_not_by_position() {
        let fields = [
            LogField::new(
                gstr!("MESSAGE"),
                b"assertion 'GDK_IS_WAYLAND_WINDOW' failed",
            ),
            LogField::new(gstr!("PRIORITY"), b"4"),
            LogField::new(gstr!("GLIB_DOMAIN"), b"Gdk"),
        ];
        assert_eq!(
            domain_and_message(&fields),
            ("Gdk", "assertion 'GDK_IS_WAYLAND_WINDOW' failed")
        );
    }

    /// A message with no domain still logs, with an empty one.
    ///
    /// GDK's `g_error` on the Wayland dispatch failure carries a domain, but plenty
    /// of what the GTK stack logs does not, and a missing field must not cost the
    /// line.
    #[test]
    fn a_missing_field_reads_as_empty_rather_than_dropping_the_message() {
        let fields = [LogField::new(gstr!("MESSAGE"), b"no domain here")];
        assert_eq!(domain_and_message(&fields), ("", "no domain here"));
    }
}
