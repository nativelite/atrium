//! Framing and output-queue rules — see the module above.

use super::*;

/// **D9.** The cap used to be checked only on the no-newline path, so a
/// request that arrived with its newline inside the final read was accepted
/// at up to MAX_REQUEST + one read (69 631 bytes for the old 4 KiB reader).
#[test]
fn framer_enforces_the_cap_on_the_newline_branch_too() {
    let mut f = Framer::new();
    // Fill to just under the limit without a newline.
    let head = vec![b'x'; MAX_REQUEST - 10];
    assert_eq!(f.feed(&head), Framed::Incomplete);
    // …then deliver a chunk whose newline sits past the limit.
    let mut tail = vec![b'y'; 4096];
    tail.push(b'\n');
    assert_eq!(
        f.feed(&tail),
        Framed::Overflow,
        "a newline beyond MAX_REQUEST must be a refusal, not an accepted request"
    );
    assert_eq!(f.buffered(), 0, "the refused body must be released");
}

/// The documented limit is the enforced limit — in both directions.
#[test]
fn framer_accepts_exactly_the_documented_limit() {
    let mut f = Framer::new();
    let mut at_limit = vec![b'x'; MAX_REQUEST];
    at_limit.push(b'\n');
    match f.feed(&at_limit) {
        Framed::Line(l) => assert_eq!(l.len(), MAX_REQUEST),
        other => panic!("a request of exactly MAX_REQUEST must be served: {other:?}"),
    }
    let mut over = vec![b'x'; MAX_REQUEST + 1];
    over.push(b'\n');
    assert_eq!(f.feed(&over), Framed::Overflow);
}

#[test]
fn framer_reassembles_a_request_split_across_reads() {
    let mut f = Framer::new();
    assert_eq!(f.feed(br#"{"cmd":"li"#), Framed::Incomplete);
    assert_eq!(f.feed(b"st"), Framed::Incomplete);
    assert_eq!(
        f.feed(b"\"}\ntrailing junk"),
        Framed::Line(r#"{"cmd":"list"}"#.to_string()),
        "the head must survive across ticks and the tail must be ignored"
    );
    assert_eq!(f.buffered(), 0);
}

#[test]
fn outqueue_tracks_partial_writes() {
    let mut q = OutQueue::new();
    q.queue_line("hello");
    assert_eq!(q.remaining(), b"hello\n");
    q.advance(2);
    assert_eq!(q.remaining(), b"llo\n");
    q.advance(99);
    assert!(q.is_empty());
}
