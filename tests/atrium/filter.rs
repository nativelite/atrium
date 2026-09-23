//! The output filter between a pane and the real terminal.

// --- passthrough filter -----------------------------------------------------

#[test]
fn filter_strips_win32_input_mode_requests() {
    use atrium::filter::Passthrough;
    let mut f = Passthrough::new();
    assert_eq!(f.feed(b"a\x1b[?9001hb\x1b[?9001lc"), b"abc".to_vec());
}

#[test]
fn filter_passes_other_escapes_untouched() {
    use atrium::filter::Passthrough;
    let mut f = Passthrough::new();
    let input = b"\x1b[31mred\x1b[?25l\x1b[?9001x\x1b[2J";
    assert_eq!(f.feed(input), input.to_vec());
}

#[test]
fn filter_survives_splits_at_every_boundary() {
    use atrium::filter::Passthrough;
    let input = b"pre\x1b[?9001hmid\x1b[?9001lpost\x1b[?900";
    for cut in 0..=input.len() {
        let mut f = Passthrough::new();
        let mut out = f.feed(&input[..cut]);
        out.extend(f.feed(&input[cut..]));
        // trailing partial candidate is held, everything else is clean
        assert_eq!(out, b"premidpost".to_vec(), "cut at {cut}");
    }
}

#[test]
fn filter_strips_alt_screen_toggles() {
    // atrium owns the alt screen: a pane's alt-buffer enter/leave — the modern
    // ?1049 and the legacy ?1047 / ?47 — must never reach the real terminal.
    use atrium::filter::Passthrough;
    let mut f = Passthrough::new();
    assert_eq!(
        f.feed(b"a\x1b[?1049hb\x1b[?1049lc\x1b[?1047hd\x1b[?1047le\x1b[?47hf\x1b[?47lg"),
        b"abcdefg".to_vec()
    );
}

#[test]
fn filter_strips_alt_screen_across_every_split() {
    // Each alt-screen sequence must survive a cut at any byte boundary; the
    // shorter ?47 has to co-exist with the longer ?1047/?1049 in the matcher.
    use atrium::filter::Passthrough;
    let input = b"pre\x1b[?1049hmid\x1b[?47lpost\x1b[?1047hend\x1b[?104";
    for cut in 0..=input.len() {
        let mut f = Passthrough::new();
        let mut out = f.feed(&input[..cut]);
        out.extend(f.feed(&input[cut..]));
        // The trailing "\x1b[?104" is an unresolved prefix, held back.
        assert_eq!(out, b"premidpostend".to_vec(), "cut at {cut}");
    }
}

#[test]
fn filter_does_not_eat_a_prefix_that_resolves_to_a_non_strip_sequence() {
    // "\x1b[?104" is a prefix of "\x1b[?1049h" but "\x1b[?104x" is not any
    // strip sequence — it must pass through intact once resolved, even across
    // a split at the ambiguous boundary.
    use atrium::filter::Passthrough;
    let input = b"\x1b[?104x";
    for cut in 0..=input.len() {
        let mut f = Passthrough::new();
        let mut out = f.feed(&input[..cut]);
        out.extend(f.feed(&input[cut..]));
        assert_eq!(out, input.to_vec(), "cut at {cut}");
    }
}
