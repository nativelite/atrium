//! What a session is hosted on: where its frames go and where its input
//! comes from.
//!
//! The loop used to hold a `rawterm::Terminal` and ask it for four things: a
//! size, a stream of keys, the mouse mode, and (through `atrium::screen`)
//! this process's stdout to paint on. [`Host`] is those four questions with
//! two answers. [`Host::Attached`] is the terminal, asked exactly as before.
//! [`Host::Headless`] is nobody: a fixed size, no keys, frames dropped, and
//! what the bar would have flashed written to stderr instead.
//!
//! Everything else in the loop is the same code on both: the panes and their
//! emulators run, the ctl channel serves, the safety net ticks, and the
//! session ends through the one teardown. A later `atrium attach` is a third
//! answer to the same questions (a sink for [`Host::screen`], a timed read for
//! [`Host::key_reader`], a size that can change for [`Host::size_watch`]); the
//! loop does not need to learn about it.

use crate::*;

/// The host of one session. See the module docs.
pub(crate) enum Host<'a> {
    /// A terminal on stdin and stdout, in raw mode for the session.
    Attached(&'a mut rawterm::Terminal),
    /// No terminal: `atrium fleet up <name> --headless`. The size is what the
    /// panes are laid out in, since there is no window to measure.
    Headless { rows: u16, cols: u16 },
}

impl Host<'_> {
    /// The size the session opens at, as `(rows, cols)`.
    pub(super) fn size(&self) -> (u16, u16) {
        match self {
            Host::Attached(term) => term.size().unwrap_or((24, 80)),
            Host::Headless { rows, cols } => (*rows, *cols),
        }
    }

    /// Where frames go: this process's stdout, or nowhere.
    pub(super) fn screen(&self) -> atrium::screen::Screen {
        match self {
            Host::Attached(_) => atrium::screen::Screen::stdout(),
            Host::Headless { .. } => atrium::screen::Screen::discard(),
        }
    }

    /// What follows the host's size: a thread asking the terminal, or the
    /// fixed size (no thread, and nothing asked of a terminal that is not
    /// there).
    pub(super) fn size_watch(&self, initial: (u16, u16)) -> atrium::screen::SizeWatch {
        match self {
            Host::Attached(_) => atrium::screen::SizeWatch::start(initial),
            Host::Headless { .. } => atrium::screen::SizeWatch::fixed(initial),
        }
    }

    /// Where input comes from: a thread reading the terminal and ringing
    /// `bell`, or nothing at all. Headless, stdin is never opened, read or
    /// polled: it may be `/dev/null`, a closed pipe or somebody else's.
    pub(super) fn key_reader(
        &self,
        bell: atrium::events::Doorbell,
    ) -> Option<atrium::events::KeyReader> {
        match self {
            Host::Attached(term) => atrium::events::KeyReader::start(term.input(), bell).ok(),
            Host::Headless { .. } => None,
        }
    }

    /// The wait at the top of a tick when there is no key thread. Attached,
    /// that is the old fallback, a timed read of the keyboard itself (`None`
    /// when the terminal is gone). Headless there is never a key thread, so
    /// this is the loop's whole wait: asleep on the doorbell the pane readers
    /// and the ctl accept thread ring, for at most a tick, and then no keys.
    /// It never returns `None`: a session nobody is attached to cannot lose
    /// its terminal.
    pub(super) fn wait_without_key_thread(
        &mut self,
        wake: &atrium::events::Wake,
    ) -> Option<Vec<u8>> {
        match self {
            Host::Attached(term) => term.read_bytes(Duration::from_millis(15)).ok(),
            Host::Headless { .. } => {
                wake.wait(LOOP_TICK);
                Some(Vec::new())
            }
        }
    }

    /// Turn the host's mouse reporting on or off. Headless there is no mouse
    /// (and no key that could ask for one).
    pub(super) fn set_mouse(&mut self, on: bool) -> std::io::Result<()> {
        match self {
            Host::Attached(term) => term.set_mouse(on),
            Host::Headless { .. } => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "a headless session has no mouse",
            )),
        }
    }

    /// Headless, a bar flash has no bar: write it to stderr as one line and
    /// clear it, so each is said once. Attached, the flash is left for the
    /// renderer, as always.
    pub(super) fn report_flash(&self, flash: &mut Option<(String, Instant)>) {
        if let Host::Headless { .. } = self {
            if let Some((note, _)) = flash.take() {
                stderr_line(&note);
            }
        }
    }

    /// Before the loop starts. Headless on Windows, take the console's control
    /// events (see [`console`]); everything else is already in place
    /// (`atrium::signals::install` ran at the top of `main`).
    pub(super) fn arm(&self) {
        #[cfg(windows)]
        if let Host::Headless { .. } = self {
            console::install();
        }
    }

    /// Whether the host is going away under the session, beyond what
    /// `atrium::signals` reports: a console control event, headless on
    /// Windows. The loop then leaves for the teardown.
    pub(super) fn ending(&self) -> bool {
        #[cfg(windows)]
        if let Host::Headless { .. } = self {
            return console::ending();
        }
        false
    }
}

/// One line on stderr, for a session with no bar: `atrium: <note>`, with
/// anything that is not printable text replaced (a note can carry a path or a
/// pane's words, and stderr is as likely a log file as a terminal). A stderr
/// that is closed is not an error worth dying for: `eprintln!` would panic
/// mid-session, so the write's result is dropped.
pub(crate) fn stderr_line(note: &str) {
    let _ = writeln!(std::io::stderr(), "{}", flash_line(note));
}

/// The line [`stderr_line`] writes for `note`. Pure.
fn flash_line(note: &str) -> String {
    let clean: String = note
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    format!("atrium: {}", atrium::fleet::sanitize(&clean))
}

/// Console control events for a headless session on Windows.
///
/// Attached, raw mode clears `ENABLE_PROCESSED_INPUT`, so Ctrl+C is a byte
/// for the focused pane and never an event. Headless there is no raw mode: a
/// Ctrl+C or Ctrl+Break in the console atrium was started from, or that
/// console closing, would end the process where it stands, with no teardown
/// (the kill-on-close job still takes the panes, but no snapshot is settled
/// and nothing is said). The handler turns each into the loop's own exit.
///
/// It lives here and is installed only for a headless host, so an attached
/// session's dispositions are exactly what they were. A process with no
/// console (a service) never sees an event; ending one is `taskkill`, which
/// no handler can catch, and the session job is the guarantee there.
#[cfg(windows)]
mod console {
    use std::sync::atomic::{AtomicBool, Ordering};

    static ENDING: AtomicBool = AtomicBool::new(false);

    /// `CTRL_C_EVENT` (0) and `CTRL_BREAK_EVENT` (1): the process goes on
    /// after the handler returns. The rest (close, logoff, shutdown) end the
    /// process as soon as it returns.
    const CTRL_BREAK_EVENT: u32 = 1;

    /// How long a close, logoff or shutdown is held off for the teardown.
    /// Windows itself allows a handler about five seconds for a close; the
    /// teardown's own waits are well under that.
    const HOLD: std::time::Duration = std::time::Duration::from_secs(10);

    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }

    /// Runs on a thread Windows creates for the event, so it may sleep.
    extern "system" fn on_event(kind: u32) -> i32 {
        ENDING.store(true, Ordering::SeqCst);
        if kind > CTRL_BREAK_EVENT {
            // The loop sees the flag within a tick and tears the session
            // down; the process then exits under this sleeping thread.
            std::thread::sleep(HOLD);
        }
        // Handled: the default handler (ExitProcess) must not run.
        1
    }

    pub(super) fn install() {
        // SAFETY: registers a handler that stores to a static and sleeps; it
        // captures nothing, and stays valid for the life of the process.
        let ok = unsafe { SetConsoleCtrlHandler(Some(on_event), 1) };
        if ok == 0 {
            super::stderr_line(
                "warning: could not take the console's control events; Ctrl+C or a closed \
                 console ends this session without its teardown",
            );
        }
    }

    pub(super) fn ending() -> bool {
        ENDING.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headless() -> Host<'static> {
        Host::Headless {
            rows: 50,
            cols: 200,
        }
    }

    #[test]
    fn a_headless_host_is_the_size_it_was_given_and_reads_no_keys() {
        let host = headless();
        assert_eq!(host.size(), (50, 200));
        assert_eq!(host.size_watch(host.size()).latest(), Some((50, 200)));
        let (bell, _wake) = atrium::events::doorbell();
        assert!(
            host.key_reader(bell).is_none(),
            "no thread may read stdin for a session nobody is attached to"
        );
        assert!(headless().set_mouse(true).is_err());
        assert!(!host.ending());
    }

    /// The idle cost of a headless loop is this wait: with nothing ringing it
    /// must sleep the tick out, not return at once (a loop that spins), and it
    /// never reports a lost terminal.
    #[test]
    fn the_headless_wait_sleeps_a_tick_when_nothing_rings() {
        let mut host = headless();
        let (_bell, wake) = atrium::events::doorbell();
        let started = Instant::now();
        let mut waits = 0u32;
        while started.elapsed() < LOOP_TICK * 10 {
            assert_eq!(host.wait_without_key_thread(&wake), Some(Vec::new()));
            waits += 1;
        }
        assert!(
            waits <= 12,
            "ten ticks of idle took {waits} passes: the wait is not sleeping"
        );
    }

    /// And a ring (pane output, a ctl connect) ends the wait at once.
    #[test]
    fn the_headless_wait_wakes_on_the_doorbell() {
        let mut host = headless();
        let (bell, wake) = atrium::events::doorbell();
        bell.ring();
        let started = Instant::now();
        assert_eq!(host.wait_without_key_thread(&wake), Some(Vec::new()));
        assert!(
            started.elapsed() < LOOP_TICK / 2,
            "a rung bell must not wait the tick out"
        );
    }

    #[test]
    fn a_headless_flash_is_said_once() {
        let mut flash = Some(("pane resize failed".to_string(), Instant::now()));
        headless().report_flash(&mut flash);
        assert!(flash.is_none(), "said once, then gone");
    }

    #[test]
    fn a_flash_line_is_one_line_with_nothing_a_log_would_execute() {
        assert_eq!(flash_line("ctl channel lost"), "atrium: ctl channel lost");
        let line = flash_line("bad\x1b[2Jpath\r\nsecond");
        assert!(!line.contains('\x1b') && !line.contains('\n') && !line.contains('\r'));
        assert!(line.starts_with("atrium: bad"));
    }
}
