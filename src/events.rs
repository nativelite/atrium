//! What wakes the run loop: a key, or output from any pane, the moment it
//! arrives.
//!
//! The loop used to wait on the keyboard for 15 ms, then poll each pane with a
//! timed read (on Windows `PeekNamedPipe` plus a sleep). A pane's output was
//! seen only when the loop came round to it, and the sleeps added up per pane:
//! measured (Windows, output timestamped on a reader thread), a key's echo took
//! a median of ~6 ms through atrium against 0.1 ms for the shell alone, and
//! ~65 ms with eight tiled panes before the per-pane wait was cut to the
//! focused pane. With this module the median is ~0.5 ms.
//!
//! Now every source has a thread that blocks on it and rings one [`Doorbell`]:
//! a key reader ([`KeyReader`], over a `rawterm::Input`) and one reader per pane
//! ([`PaneInbox`], over a `pty::PtyReader`). The loop waits on the doorbell
//! ([`Wake::wait`]) with a timeout for its timed work, and on waking takes what
//! arrived without blocking.
//!
//! A pane's inbox is bounded, so a pane that outpaces the loop blocks on its own
//! output exactly as it did when the loop read its pipe directly.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

/// Bytes per pane read; matches the run loop's buffer, so a chunk always fits.
pub const CHUNK: usize = 8192;

/// Chunks a pane may queue ahead of the loop before its reader waits (~2 MiB).
pub const INBOX_CHUNKS: usize = 256;

/// Rung by any reader when it has something for the loop. Cheap to clone and
/// lossy on purpose: one pending ring is enough to wake the loop, which then
/// takes everything that arrived.
#[derive(Clone)]
pub struct Doorbell {
    tx: SyncSender<()>,
}

impl Doorbell {
    pub fn ring(&self) {
        let _ = self.tx.try_send(());
    }
}

/// The loop's side of the doorbell.
pub struct Wake {
    rx: Receiver<()>,
}

impl Wake {
    /// Sleep until a reader rings or `timeout` passes, whichever is first.
    pub fn wait(&self, timeout: Duration) {
        if self.rx.recv_timeout(timeout).is_ok() {
            // Collapse rings that piled up while the loop was busy.
            while self.rx.try_recv().is_ok() {}
        }
    }
}

/// A doorbell and its waker.
pub fn doorbell() -> (Doorbell, Wake) {
    let (tx, rx) = mpsc::sync_channel(1);
    (Doorbell { tx }, Wake { rx })
}

/// What one take from a pane's inbox produced.
#[derive(Debug, PartialEq, Eq)]
pub enum PaneRead {
    /// Output, copied into the caller's buffer: this many bytes.
    Bytes(usize),
    /// The pane's output ended (the child side closed).
    Ended,
    /// Nothing waiting right now.
    Empty,
}

/// A pane's output, delivered by a reader thread.
///
/// Holds only the receiving end: dropping the inbox makes the reader's next
/// delivery fail, and the thread exits. **Drop it before the pane's `Pty`**: on
/// Windows dropping a `Pty` waits for the console host to finish writing, and a
/// reader blocked on a full inbox would never let it.
pub struct PaneInbox {
    rx: Receiver<Vec<u8>>,
}

impl PaneInbox {
    /// Start reading `reader` on a thread of its own, ringing `bell` with each
    /// chunk and at the end.
    pub fn start(reader: pty::PtyReader, bell: Doorbell) -> io::Result<PaneInbox> {
        Self::start_with(reader, bell)
    }

    /// [`start`](Self::start) over any byte source (tests use a pipe-like fake).
    pub fn start_with<R: io::Read + Send + 'static>(
        mut reader: R,
        bell: Doorbell,
    ) -> io::Result<PaneInbox> {
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(INBOX_CHUNKS);
        std::thread::Builder::new()
            .name("atrium-pane-read".to_string())
            .spawn(move || {
                let mut buf = [0u8; CHUNK];
                loop {
                    let n = reader.read(&mut buf).unwrap_or(0);
                    // An empty chunk is the end marker.
                    let chunk = buf[..n].to_vec();
                    let sent = tx.send(chunk).is_ok();
                    bell.ring();
                    if n == 0 || !sent {
                        return;
                    }
                }
            })?;
        Ok(PaneInbox { rx })
    }

    /// Take the next chunk into `buf` without waiting.
    pub fn take(&self, buf: &mut [u8]) -> PaneRead {
        match self.rx.try_recv() {
            Ok(chunk) if chunk.is_empty() => PaneRead::Ended,
            Ok(chunk) => {
                // Chunks are at most CHUNK bytes; a shorter caller buffer would
                // lose the tail, so it is a programming error, caught in tests.
                debug_assert!(chunk.len() <= buf.len());
                let n = chunk.len().min(buf.len());
                buf[..n].copy_from_slice(&chunk[..n]);
                PaneRead::Bytes(n)
            }
            Err(TryRecvError::Empty) => PaneRead::Empty,
            // The reader thread is gone without an end marker (it panicked):
            // treat it as the end rather than a pane that never speaks again.
            Err(TryRecvError::Disconnected) => PaneRead::Ended,
        }
    }
}

/// Keys, delivered by a reader thread.
pub struct KeyReader {
    rx: Receiver<io::Result<Vec<u8>>>,
    /// The reader hit an error; reported on the next `take`.
    failed: std::cell::Cell<bool>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl KeyReader {
    /// How long the thread blocks per read before checking whether to stop.
    const POLL: Duration = Duration::from_millis(50);

    /// Start reading `input` on a thread of its own, ringing `bell` per read.
    pub fn start(input: rawterm::Input, bell: Doorbell) -> io::Result<KeyReader> {
        let mut input = input;
        Self::start_with(move |t| input.read_bytes(t), bell)
    }

    /// [`start`](Self::start) over any timed read (tests use a fake).
    pub fn start_with<F>(mut read: F, bell: Doorbell) -> io::Result<KeyReader>
    where
        F: FnMut(Duration) -> io::Result<Vec<u8>> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::Builder::new()
            .name("atrium-keys".to_string())
            .spawn(move || {
                while !stopping.load(Ordering::SeqCst) {
                    match read(Self::POLL) {
                        Ok(bytes) if bytes.is_empty() => {}
                        Ok(bytes) => {
                            if tx.send(Ok(bytes)).is_err() {
                                return;
                            }
                            bell.ring();
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            bell.ring();
                            return;
                        }
                    }
                }
            })?;
        Ok(KeyReader {
            rx,
            failed: std::cell::Cell::new(false),
            stop,
            thread: Some(thread),
        })
    }

    /// Every key read since the last call, in order, without waiting. An error
    /// means the terminal is gone; keys read before it are returned first and
    /// the error on the next call.
    pub fn take(&self) -> io::Result<Vec<u8>> {
        if self.failed.get() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "terminal input ended",
            ));
        }
        let mut out = Vec::new();
        for got in self.rx.try_iter() {
            match got {
                Ok(bytes) => out.extend_from_slice(&bytes),
                Err(e) if out.is_empty() => {
                    self.failed.set(true);
                    return Err(e);
                }
                Err(_) => {
                    self.failed.set(true);
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Stop the thread and wait for its current read (at most [`Self::POLL`]).
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for KeyReader {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// A byte source fed from the test, blocking like a pipe until it has data.
    struct FakePipe(Receiver<Vec<u8>>);

    impl io::Read for FakePipe {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.recv() {
                Ok(b) => {
                    buf[..b.len()].copy_from_slice(&b);
                    Ok(b.len())
                }
                Err(_) => Ok(0),
            }
        }
    }

    #[test]
    fn pane_output_wakes_the_loop_at_once_and_arrives_in_order() {
        let (bell, wake) = doorbell();
        let (feed, pipe) = mpsc::channel();
        let inbox = PaneInbox::start_with(FakePipe(pipe), bell).unwrap();
        let mut buf = [0u8; CHUNK];
        assert_eq!(inbox.take(&mut buf), PaneRead::Empty);

        let asleep = Instant::now();
        let feeder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            feed.send(b"one".to_vec()).unwrap();
            feed.send(b"two".to_vec()).unwrap();
            feed
        });
        wake.wait(Duration::from_secs(5));
        let woke = asleep.elapsed();
        assert!(
            woke < Duration::from_secs(1),
            "the doorbell must end the wait early ({woke:?})"
        );

        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while got.len() < 6 && Instant::now() < deadline {
            match inbox.take(&mut buf) {
                PaneRead::Bytes(n) => got.extend_from_slice(&buf[..n]),
                PaneRead::Empty => std::thread::sleep(Duration::from_millis(5)),
                PaneRead::Ended => panic!("ended early"),
            }
        }
        assert_eq!(got, b"onetwo");

        // Closing the source ends the inbox.
        drop(feeder.join().unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ended = false;
        while Instant::now() < deadline {
            if inbox.take(&mut buf) == PaneRead::Ended {
                ended = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ended);
    }

    #[test]
    fn keys_arrive_in_order_and_a_dead_terminal_is_an_error() {
        let (bell, wake) = doorbell();
        let script = Arc::new(std::sync::Mutex::new(vec![
            Ok(b"a".to_vec()),
            Ok(Vec::new()),
            Ok(b"bc".to_vec()),
            Err(io::Error::new(io::ErrorKind::Other, "gone")),
        ]));
        let feed = script.clone();
        let mut keys = KeyReader::start_with(
            move |_| {
                let mut s = feed.lock().unwrap();
                if s.is_empty() {
                    std::thread::sleep(Duration::from_millis(5));
                    Ok(Vec::new())
                } else {
                    s.remove(0)
                }
            },
            bell,
        )
        .unwrap();
        wake.wait(Duration::from_secs(5));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got = Vec::new();
        let mut failed = false;
        while Instant::now() < deadline && !failed {
            match keys.take() {
                Ok(b) => got.extend_from_slice(&b),
                Err(_) => failed = true,
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(failed, "an input error must reach the loop");
        assert_eq!(got, b"abc", "keys before the error are not lost");
        keys.stop();
    }
}
