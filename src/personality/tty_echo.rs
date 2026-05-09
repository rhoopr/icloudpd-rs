//! Suppress the terminal driver's `^C` echo while a friendly bar is on screen.
//!
//! Indicatif fills the last bar line with spaces up to the terminal's right
//! edge so that "next user writes/prints will happen on the next line"
//! (`indicatif/src/draw_target.rs:579-582`). When the user hits Ctrl+C the
//! tty driver writes `^C` at that position, the `^` overflows column N, and
//! the terminal wraps to a new line below the bar. Indicatif's next steady
//! tick then does `cursor_up(bar_count - 1)`, lands one line *inside* the
//! bar, and clears the wrong N lines - leaving the original top rule as a
//! ghost above the live bar.
//!
//! Clearing `ECHOCTL` makes the tty driver not visualize control characters
//! as `^X`. The Ctrl+C keypress still delivers SIGINT (that's `ISIG`, a
//! separate flag); only the visible echo is suppressed. Restored on Drop
//! so a normal exit leaves the user's terminal as we found it.
//!
//! Unix-only. Windows console doesn't echo `^C` and the bug doesn't
//! reproduce there, so the guard is a no-op.

#[cfg(unix)]
mod platform {
    use std::sync::Mutex;

    use libc::{tcgetattr, tcsetattr, termios, ECHOCTL, STDIN_FILENO, TCSANOW};

    /// Saved `c_lflag` from the call to `install`. Held in a `Mutex<Option<...>>`
    /// so a manual `restore_now()` from the shutdown path and the `Drop` of
    /// the guard cooperate without racing or double-restoring.
    static SAVED: Mutex<Option<libc::tcflag_t>> = Mutex::new(None);

    /// RAII guard: clears `ECHOCTL` on `install`, restores the original
    /// `c_lflag` on `Drop`. Returns `None` if the fd isn't a tty, the
    /// `tcgetattr` call fails, or a guard is already installed. The
    /// constructor is gated behind this private module so callers can't
    /// build one without going through `install`.
    pub struct EchoGuard;

    impl EchoGuard {
        pub fn install() -> Option<Self> {
            let mut saved = SAVED.lock().ok()?;
            if saved.is_some() {
                return None;
            }
            // SAFETY: `termios` is plain old data; zero-init is sound
            // because `tcgetattr` either overwrites every field on success
            // or fails and we abort.
            let mut t: termios = unsafe { std::mem::zeroed() };
            // SAFETY: `STDIN_FILENO` is a valid fd for the duration of the
            // process; `&mut t` is a valid out-pointer.
            if unsafe { tcgetattr(STDIN_FILENO, &mut t) } != 0 {
                return None;
            }
            let original = t.c_lflag;
            t.c_lflag &= !ECHOCTL;
            // SAFETY: same fd validity as above; `&t` is a valid in-pointer.
            if unsafe { tcsetattr(STDIN_FILENO, TCSANOW, &t) } != 0 {
                return None;
            }
            *saved = Some(original);
            Some(Self)
        }
    }

    impl Drop for EchoGuard {
        fn drop(&mut self) {
            restore_now();
        }
    }

    /// Restore the saved `c_lflag` immediately. Idempotent: a no-op if no
    /// guard is installed or if a previous call already restored. Called
    /// from the shutdown handler before `std::process::exit` so a forced
    /// exit doesn't leave the user's terminal with `ECHOCTL` cleared.
    pub fn restore_now() {
        let Ok(mut saved) = SAVED.lock() else {
            return;
        };
        let Some(original) = saved.take() else {
            return;
        };
        // SAFETY: `termios` POD; zero-init overwritten by tcgetattr.
        let mut t: termios = unsafe { std::mem::zeroed() };
        // SAFETY: STDIN_FILENO is the live process fd; `&mut t` valid
        // out-pointer. Failures are ignored: no recovery is possible and
        // the user's shell will reset the tty on the next prompt.
        if unsafe { tcgetattr(STDIN_FILENO, &mut t) } != 0 {
            return;
        }
        t.c_lflag = original;
        // SAFETY: same fd validity; `&t` valid in-pointer.
        let _ = unsafe { tcsetattr(STDIN_FILENO, TCSANOW, &t) };
    }
}

#[cfg(not(unix))]
mod platform {
    pub struct EchoGuard;

    impl EchoGuard {
        pub fn install() -> Option<Self> {
            None
        }
    }

    pub fn restore_now() {}
}

pub use platform::{restore_now, EchoGuard};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_without_install_is_a_noop() {
        // Idempotent restore: safe to call even when no guard was installed
        // (test runners typically run without a controlling tty).
        restore_now();
        restore_now();
    }

    #[cfg(unix)]
    #[test]
    fn install_returns_none_when_stdin_is_not_a_tty() {
        // Cargo's test harness redirects stdin away from any tty, so
        // tcgetattr fails with ENOTTY. The guard must report that cleanly
        // instead of panicking or modifying terminal state.
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() {
            // If the developer is running this with stdin still on a tty
            // (rare; happens with `cargo test -- --nocapture` interactively),
            // skip rather than mutate their session.
            return;
        }
        assert!(EchoGuard::install().is_none());
    }
}
