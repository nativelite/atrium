// ---------------------------------------------------------------------------
// winmap: documented Win32 return triples -> the `chan` seam's vocabulary.
//
// Compiled on every platform on purpose. These functions are where the Windows
// defects lived (a SUCCESS/zero-bytes write treated as delivered, and
// ERROR_MORE_DATA treated as "nothing arrived" while discarding the bytes it had
// just filled in); keeping them pure means the mac that cannot run Win32 still
// runs their tests.
// ---------------------------------------------------------------------------
use super::chan::{Recv, Sent};

pub const ERROR_BROKEN_PIPE: i32 = 109;
pub const ERROR_NO_DATA: i32 = 232;
pub const ERROR_PIPE_NOT_CONNECTED: i32 = 233;
pub const ERROR_MORE_DATA: i32 = 234;
pub const ERROR_PIPE_CONNECTED: i32 = 535;
pub const ERROR_PIPE_LISTENING: i32 = 536;

/// What `ConnectNamedPipe` on a `PIPE_NOWAIT` instance means.
#[derive(Debug, PartialEq, Eq)]
pub enum Accepted {
    /// A client is on the line.
    Connected,
    /// Nobody yet — the ordinary idle path.
    Listening,
    /// The client came and went; recycle the instance.
    Recycle,
}

pub fn connect_outcome(ok: i32, err: i32) -> Accepted {
    if ok != 0 {
        return Accepted::Connected;
    }
    match err {
        ERROR_PIPE_CONNECTED => Accepted::Connected,
        ERROR_NO_DATA | ERROR_BROKEN_PIPE => Accepted::Recycle,
        _ => Accepted::Listening,
    }
}

/// Classify `ReadFile`.
///
/// The bug this replaces: the gate was `ok != 0 && n > 0`, so
/// `ERROR_MORE_DATA` (234) — which reports `ok == 0` *with the buffer
/// already filled* — fell into the error arm, returned "nothing arrived",
/// and threw away the 8 192 bytes it had. The tail of the message then read
/// back as a successful read, so the run loop was handed a fragment that
/// started mid-JSON. Those bytes are data; they must be kept and framed.
pub fn read_outcome(ok: i32, n: usize, err: i32) -> Recv {
    if ok != 0 {
        return if n > 0 {
            Recv::Data(n)
        } else {
            Recv::WouldBlock
        };
    }
    match err {
        ERROR_MORE_DATA => {
            if n > 0 {
                Recv::Data(n)
            } else {
                Recv::WouldBlock
            }
        }
        ERROR_NO_DATA | ERROR_PIPE_LISTENING => Recv::WouldBlock,
        _ => Recv::Closed,
    }
}

/// Classify `WriteFile`.
///
/// The bug this replaces: only `ok == 0` was treated as failure. On a
/// non-blocking pipe whose buffer cannot take the whole write, `WriteFile`
/// returns **SUCCESS having written zero bytes** — so the reply was dropped
/// on the floor and the client disconnected, with no error anywhere. The
/// `written` out-param is the only truth here.
pub fn write_outcome(ok: i32, written: usize, err: i32) -> Sent {
    if ok != 0 {
        return if written > 0 {
            Sent::Wrote(written)
        } else {
            Sent::WouldBlock
        };
    }
    match err {
        ERROR_NO_DATA if written > 0 => Sent::Wrote(written),
        ERROR_NO_DATA => Sent::WouldBlock,
        _ => Sent::Failed,
    }
}

/// Has the client closed its end? Used while draining a written reply, in
/// place of `FlushFileBuffers` — which, on the server end of a named pipe,
/// "does not return until the client has read all buffered data", i.e. it
/// blocks the single run loop for as long as a client feels like not reading.
pub fn drain_outcome(ok: i32, n: usize, err: i32) -> bool {
    let _ = (ok, n);
    err == ERROR_BROKEN_PIPE || err == ERROR_PIPE_NOT_CONNECTED
}

#[cfg(test)]
mod tests;
