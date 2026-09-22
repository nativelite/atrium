//! Documented Win32 return triples -> the `chan` seam.

use super::*;
use crate::ipc::chan::{Recv, Sent};
use crate::ipc::winmap;

/// **D4.** `ERROR_MORE_DATA` (234) reports `ok == 0` *with the buffer
/// already filled*. The old gate (`ok != 0 && n > 0`) sent it to the error
/// arm, which returned "nothing arrived" and dropped the 8 192 bytes it was
/// holding.
#[test]
fn win_read_more_data_keeps_the_bytes() {
    assert_eq!(
        winmap::read_outcome(0, 8192, ERROR_MORE_DATA),
        Recv::Data(8192),
        "ERROR_MORE_DATA carries data; discarding it loses the head of the request"
    );
    assert_eq!(winmap::read_outcome(1, 3671, 0), Recv::Data(3671));
    assert_eq!(winmap::read_outcome(0, 0, ERROR_NO_DATA), Recv::WouldBlock);
    assert_eq!(
        winmap::read_outcome(0, 0, ERROR_PIPE_LISTENING),
        Recv::WouldBlock
    );
    assert_eq!(winmap::read_outcome(0, 0, ERROR_BROKEN_PIPE), Recv::Closed);
    assert_eq!(
        winmap::read_outcome(0, 0, ERROR_PIPE_NOT_CONNECTED),
        Recv::Closed
    );
}

/// **D3.** On a non-blocking pipe that cannot take the write, `WriteFile`
/// returns SUCCESS having written ZERO bytes. Only `ok == 0` used to be
/// treated as failure, so the reply was dropped and the client disconnected
/// with nothing to show for it.
#[test]
fn win_write_success_with_zero_written_is_not_delivery() {
    assert_eq!(
        winmap::write_outcome(1, 0, 0),
        Sent::WouldBlock,
        "SUCCESS with zero bytes written means the pipe took nothing"
    );
    assert_eq!(winmap::write_outcome(1, 4096, 0), Sent::Wrote(4096));
    assert_eq!(winmap::write_outcome(0, 0, ERROR_NO_DATA), Sent::WouldBlock);
    assert_eq!(winmap::write_outcome(0, 0, ERROR_BROKEN_PIPE), Sent::Failed);
}

#[test]
fn win_connect_and_drain_outcomes() {
    assert_eq!(winmap::connect_outcome(1, 0), Accepted::Connected);
    assert_eq!(
        winmap::connect_outcome(0, ERROR_PIPE_CONNECTED),
        Accepted::Connected
    );
    assert_eq!(
        winmap::connect_outcome(0, ERROR_PIPE_LISTENING),
        Accepted::Listening
    );
    assert_eq!(winmap::connect_outcome(0, ERROR_NO_DATA), Accepted::Recycle);
    // The drain probe that replaces FlushFileBuffers: only a hung-up client
    // means "everything was read".
    assert!(winmap::drain_outcome(0, 0, ERROR_BROKEN_PIPE));
    assert!(!winmap::drain_outcome(0, 0, ERROR_NO_DATA));
}
