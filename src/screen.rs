//! The terminal as a queue, so a terminal that stops reading freezes nothing
//! but its own view.
//!
//! atrium used to write every frame straight to stdout from the run loop. When
//! the terminal stops reading — a stopped or wedged terminal emulator, a
//! console with a QuickEdit selection held — that write blocks, and the whole
//! loop blocked with it: no keystrokes reached the panes, no pane output was
//! drained (so the panes blocked on their own output too), no control-plane
//! request was answered.
//!
//! [`Screen`] is the loop's `Write` instead. Bytes accumulate until `flush`,
//! and each flush is handed to a writer thread as one chunk; the loop never
//! waits on the terminal. If the terminal falls more than [`Screen::limit`]
//! bytes behind, the screen stops queueing and discards, because nothing is
//! lost by it: every pane's emulator holds its current screen. Once the writer
//! has caught up, [`Screen::take_resync`] tells the loop to repaint everything
//! from those emulators, and the view is whole again.
//!
//! Chunks are dropped whole, never split, and a resync starts by cancelling
//! anything the terminal might still be parsing (see [`RESYNC_PREFIX`]).

use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// How far the terminal may fall behind before the screen stops queueing.
///
/// Small on purpose. Everything queued is replayed when the terminal reads
/// again, and replaying stale frames is worse than useless: measured with a
/// 4 MiB limit, a terminal that resumed was still scrolling through old output
/// five seconds later. Past this much, skipping to a repaint of the current
/// view is what a person wants — a fast terminal never gets near it.
pub const DEFAULT_LIMIT: usize = 256 * 1024;

/// Written first on a resync, so the repaint lands on a terminal in a known
/// state whatever the last delivered chunk left half-parsed: CAN aborts an
/// escape or control sequence, ST ends a string (OSC, DCS), then synchronized
/// output is closed, attributes reset and the alternate screen re-entered.
pub const RESYNC_PREFIX: &[u8] = b"\x18\x1b\\\x1b[?2026l\x1b[0m\x1b[?1049h";

/// A non-blocking terminal writer. See the module docs.
pub struct Screen {
    buf: Vec<u8>,
    tx: Option<mpsc::Sender<Vec<u8>>>,
    /// The sink itself, written synchronously, if the writer thread could not
    /// be started.
    fallback: Option<Box<dyn Write + Send>>,
    /// Bytes handed to the writer thread and not yet written.
    pending: Arc<AtomicUsize>,
    /// Set when a chunk was discarded; cleared by `take_resync`.
    dropping: bool,
    limit: usize,
    /// Signalled by the writer thread when it has written everything and exits.
    done: mpsc::Receiver<()>,
}

impl Screen {
    /// A screen over this process's stdout.
    pub fn stdout() -> Screen {
        Screen::new(io::stdout(), DEFAULT_LIMIT)
    }

    /// A screen over any sink, discarding once `limit` bytes are queued.
    pub fn new<W: Write + Send + 'static>(sink: W, limit: usize) -> Screen {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let (done_tx, done) = mpsc::channel();
        let pending = Arc::new(AtomicUsize::new(0));
        let written = pending.clone();
        // The sink waits in a slot the thread empties when it starts, so a
        // thread that never starts leaves it here for the synchronous fallback.
        let slot: Arc<std::sync::Mutex<Option<Box<dyn Write + Send>>>> =
            Arc::new(std::sync::Mutex::new(Some(Box::new(sink))));
        let taken = slot.clone();
        let spawned = std::thread::Builder::new()
            .name("atrium-screen".to_string())
            .spawn(move || {
                let Some(mut sink) = taken.lock().ok().and_then(|mut s| s.take()) else {
                    return;
                };
                // `while let` rather than `for`, so the loop can also take
                // whatever else is queued (below) without consuming `rx`.
                while let Ok(chunk) = rx.recv() {
                    // Take everything already queued and write it once. A tick
                    // can queue several chunks (the passthrough drain, then the
                    // composite and bar), and on Windows *every* write to the
                    // console host opens a ~16 ms frame window in which a key
                    // echo waits — so the write count, not just the byte count,
                    // is latency. This never waits for more: it takes only what
                    // is already there.
                    let mut batch = chunk;
                    while let Ok(more) = rx.try_recv() {
                        batch.extend_from_slice(&more);
                    }
                    let _ = sink.write_all(&batch);
                    let _ = sink.flush();
                    written.fetch_sub(batch.len(), Ordering::SeqCst);
                }
                let _ = done_tx.send(());
            });
        let fallback = match spawned {
            Ok(_) => None,
            // No thread: write synchronously, as atrium always did, rather than
            // show nothing at all.
            Err(_) => slot.lock().ok().and_then(|mut s| s.take()),
        };
        Screen {
            buf: Vec::new(),
            tx: fallback.is_none().then_some(tx),
            fallback,
            pending,
            dropping: false,
            limit,
            done,
        }
    }

    /// The discard threshold in bytes.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Bytes queued for the terminal and not yet written.
    pub fn backlog(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }

    /// Whether output has been discarded since the last resync.
    pub fn is_dropping(&self) -> bool {
        self.dropping
    }

    /// True once, when output was discarded and the terminal has since caught
    /// up: the caller must now repaint the whole view. The repaint starts with
    /// [`RESYNC_PREFIX`], already queued here.
    pub fn take_resync(&mut self) -> bool {
        if !self.dropping || self.backlog() > 0 {
            return false;
        }
        self.dropping = false;
        let mut chunk = RESYNC_PREFIX.to_vec();
        chunk.append(&mut self.buf);
        self.buf = chunk;
        true
    }

    /// Hand everything to the writer and wait up to `timeout` for the terminal
    /// to take it. Used once, at exit, so the restore sequences are delivered
    /// even if output was being discarded; a terminal that never reads again
    /// can't hold atrium open past `timeout`.
    pub fn finish(mut self, timeout: Duration) {
        if self.dropping {
            self.dropping = false;
            let mut chunk = RESYNC_PREFIX.to_vec();
            chunk.append(&mut self.buf);
            self.buf = chunk;
        }
        self.send();
        if self.tx.take().is_some() {
            let _ = self.done.recv_timeout(timeout);
        }
    }

    fn send(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let chunk = std::mem::take(&mut self.buf);
        if let Some(sink) = self.fallback.as_mut() {
            let _ = sink.write_all(&chunk);
            let _ = sink.flush();
            return;
        }
        let Some(tx) = &self.tx else {
            return;
        };
        self.pending.fetch_add(chunk.len(), Ordering::SeqCst);
        let len = chunk.len();
        if tx.send(chunk).is_err() {
            self.pending.fetch_sub(len, Ordering::SeqCst);
        }
    }
}

impl Write for Screen {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    /// Queue what was written since the last flush as one chunk — or discard
    /// it, while the terminal is too far behind. Never blocks.
    fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        if self.dropping || self.backlog() + self.buf.len() > self.limit {
            self.dropping = true;
            self.buf.clear();
            return Ok(());
        }
        self.send();
        Ok(())
    }
}

/// The terminal size, polled on a thread of its own.
///
/// Measured on Windows: with the terminal not reading, the run loop's size
/// query (`GetConsoleScreenBufferInfo`) did not return for 63 s. The console
/// host answers no call while it is blocked delivering output, so a queued
/// writer alone left the loop frozen at its next size check. The watcher asks
/// instead and the loop reads the last answer, which never blocks.
pub struct SizeWatch {
    /// `rows << 16 | cols` of the last answer; 0 until there is one.
    latest: Arc<std::sync::atomic::AtomicU32>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl SizeWatch {
    /// How often the size is asked for.
    pub const EVERY: Duration = Duration::from_millis(150);

    /// Start watching this process's terminal, seeded with `initial`.
    pub fn start(initial: (u16, u16)) -> SizeWatch {
        let latest = Arc::new(std::sync::atomic::AtomicU32::new(pack(initial)));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (store, stopping) = (latest.clone(), stop.clone());
        // Not joined on drop: the thread may be parked inside the very call
        // this exists to keep off the loop. It notices `stop` once that returns.
        let _ = std::thread::Builder::new()
            .name("atrium-size".to_string())
            .spawn(move || {
                while !stopping.load(Ordering::SeqCst) {
                    if let Ok(size) = rawterm::size() {
                        store.store(pack(size), Ordering::SeqCst);
                    }
                    std::thread::sleep(Self::EVERY);
                }
            });
        SizeWatch { latest, stop }
    }

    /// The most recent size as `(rows, cols)`, or `None` if none is known.
    pub fn latest(&self) -> Option<(u16, u16)> {
        unpack(self.latest.load(Ordering::SeqCst))
    }
}

impl Drop for SizeWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn pack((rows, cols): (u16, u16)) -> u32 {
    (rows as u32) << 16 | cols as u32
}

fn unpack(v: u32) -> Option<(u16, u16)> {
    (v != 0).then_some(((v >> 16) as u16, v as u16))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Condvar, Mutex};

    /// A sink that blocks every write until the test opens its gate — a
    /// terminal that has stopped reading.
    #[derive(Clone)]
    struct Gated {
        open: Arc<(Mutex<bool>, Condvar)>,
        got: Arc<Mutex<Vec<u8>>>,
    }

    impl Gated {
        fn new() -> Gated {
            Gated {
                open: Arc::new((Mutex::new(false), Condvar::new())),
                got: Arc::new(Mutex::new(Vec::new())),
            }
        }
        fn open(&self) {
            *self.open.0.lock().unwrap() = true;
            self.open.1.notify_all();
        }
        fn got(&self) -> Vec<u8> {
            self.got.lock().unwrap().clone()
        }
    }

    impl Write for Gated {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            let mut open = self.open.0.lock().unwrap();
            while !*open {
                open = self.open.1.wait(open).unwrap();
            }
            self.got.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn wait_for(mut f: impl FnMut() -> bool) -> bool {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < end {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn a_stuck_terminal_never_blocks_the_writer_and_the_view_resyncs() {
        let sink = Gated::new();
        let mut s = Screen::new(sink.clone(), 64);
        let started = std::time::Instant::now();
        // Far more than the limit, with the terminal not reading at all.
        for i in 0..1000 {
            write!(s, "frame {i:04} ").unwrap();
            s.flush().unwrap();
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "writing to a stuck terminal blocked the caller"
        );
        assert!(s.is_dropping(), "past the limit the screen discards");
        assert!(
            !s.take_resync(),
            "no resync while the terminal is still behind"
        );

        sink.open();
        assert!(
            wait_for(|| s.backlog() == 0),
            "the writer drains once reading resumes"
        );
        assert!(s.take_resync(), "caught up after discarding: repaint");
        assert!(!s.take_resync(), "the resync is handed out once");
        write!(s, "REPAINT").unwrap();
        s.flush().unwrap();
        s.finish(Duration::from_secs(5));

        let got = sink.got();
        let at = got
            .windows(RESYNC_PREFIX.len())
            .position(|w| w == RESYNC_PREFIX)
            .expect("the repaint is prefixed");
        assert!(
            got[at..].ends_with(b"REPAINT"),
            "the repaint follows the prefix"
        );
        // Whole chunks only: everything delivered before the resync is intact
        // frames, never a torn one.
        let before = String::from_utf8(got[..at].to_vec()).unwrap();
        assert!(before
            .split_terminator(' ')
            .all(|w| w == "frame" || w.len() == 4));
    }

    /// A sink that keeps each write separate, and blocks in the first one until
    /// the test lets go — so the test controls what is queued behind it.
    #[derive(Clone)]
    struct Batches {
        open: Arc<(Mutex<bool>, Condvar)>,
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        blocked: Arc<Mutex<bool>>,
    }

    impl Batches {
        fn new() -> Batches {
            Batches {
                open: Arc::new((Mutex::new(false), Condvar::new())),
                writes: Arc::new(Mutex::new(Vec::new())),
                blocked: Arc::new(Mutex::new(false)),
            }
        }
        fn open(&self) {
            *self.open.0.lock().unwrap() = true;
            self.open.1.notify_all();
        }
        fn writes(&self) -> Vec<Vec<u8>> {
            self.writes.lock().unwrap().clone()
        }
        fn is_blocked(&self) -> bool {
            *self.blocked.lock().unwrap()
        }
    }

    impl Write for Batches {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            let mut open = self.open.0.lock().unwrap();
            *self.blocked.lock().unwrap() = true;
            while !*open {
                open = self.open.1.wait(open).unwrap();
            }
            *self.blocked.lock().unwrap() = false;
            self.writes.lock().unwrap().push(b.to_vec());
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Frames that piled up while the terminal was busy go out as one write, not
    /// one write each: on Windows every write to the console host costs the next
    /// keystroke up to a frame (~16 ms).
    #[test]
    fn frames_queued_together_are_written_together() {
        let sink = Batches::new();
        let mut s = Screen::new(sink.clone(), DEFAULT_LIMIT);
        write!(s, "first").unwrap();
        s.flush().unwrap();
        assert!(
            wait_for(|| sink.is_blocked()),
            "the writer should be inside the first write"
        );
        // Both queue up behind the write in progress.
        write!(s, "second").unwrap();
        s.flush().unwrap();
        write!(s, "third").unwrap();
        s.flush().unwrap();
        // The blocked write still counts: its bytes are subtracted when it
        // returns, not when it starts.
        assert!(
            wait_for(|| s.backlog() == b"firstsecondthird".len()),
            "both frames are queued behind the write in progress"
        );

        sink.open();
        assert!(wait_for(|| s.backlog() == 0), "the writer drains");
        s.finish(Duration::from_secs(5));
        assert_eq!(
            sink.writes(),
            vec![b"first".to_vec(), b"secondthird".to_vec()],
            "the two queued frames are one write"
        );
    }

    #[test]
    fn a_reading_terminal_gets_every_byte_in_order() {
        let sink = Gated::new();
        sink.open();
        let mut s = Screen::new(sink.clone(), DEFAULT_LIMIT);
        let mut want = Vec::new();
        for i in 0..500 {
            let line = format!("line {i}\r\n");
            want.extend_from_slice(line.as_bytes());
            s.write_all(line.as_bytes()).unwrap();
            s.flush().unwrap();
        }
        s.finish(Duration::from_secs(5));
        assert_eq!(sink.got(), want);
    }

    #[test]
    fn finish_delivers_the_restore_even_after_discarding() {
        let sink = Gated::new();
        let mut s = Screen::new(sink.clone(), 8);
        for _ in 0..10 {
            s.write_all(b"0123456789").unwrap();
            s.flush().unwrap();
        }
        assert!(s.is_dropping());
        s.write_all(b"RESTORE").unwrap();
        let _ = s.flush(); // discarded like any frame while behind
        s.write_all(b"RESTORE").unwrap();
        sink.open();
        s.finish(Duration::from_secs(5));
        assert!(sink.got().ends_with(b"RESTORE"), "{:?}", sink.got());
    }

    #[test]
    fn a_size_round_trips_and_unknown_is_none() {
        assert_eq!(unpack(pack((50, 211))), Some((50, 211)));
        assert_eq!(unpack(pack((u16::MAX, 1))), Some((u16::MAX, 1)));
        assert_eq!(unpack(0), None);
        // Seeded before the thread has answered anything.
        assert_eq!(
            SizeWatch::start((24, 80)).latest().map(|s| s.1 > 0),
            Some(true)
        );
    }

    #[test]
    fn finish_gives_up_on_a_terminal_that_never_reads() {
        let sink = Gated::new();
        let mut s = Screen::new(sink, DEFAULT_LIMIT);
        s.write_all(b"never read").unwrap();
        s.flush().unwrap();
        let started = std::time::Instant::now();
        s.finish(Duration::from_millis(200));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
