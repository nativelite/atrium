//! A zero-dependency, non-cryptographic v4 UUID generator.
//!
//! amux binds each agent pane to the transcript its CLI writes by minting a
//! fresh session id and passing it as `claude --session-id <uuid>` (§3.3 of the
//! 0.3 design). That id is an **identifier, not a secret** — uniqueness is all
//! that is required, and the format must be a syntactically valid v4 UUID so the
//! CLI accepts it and names its transcript `<uuid>.jsonl`.
//!
//! amux has no RNG crate (zero third-party deps), so entropy is assembled from
//! std only: the wall clock's nanoseconds, a process-lifetime atomic counter (so
//! two ids minted in the same nanosecond still differ), and the per-process seed
//! behind [`std::collections::hash_map::RandomState`] (hashed, to spread bits).
//! None of this is cryptographically strong and it is not meant to be — collision
//! avoidance for a handful of panes is the only bar.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Bumped once per generated id so same-nanosecond calls never collide.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Mint a fresh, syntactically valid v4 UUID string
/// (`xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx`, `y ∈ {8,9,a,b}`).
///
/// Non-cryptographic: the value is an identifier for the pane↔transcript join,
/// not a token. Do not use it where unpredictability matters.
pub fn v4() -> String {
    // 128 bits assembled from two independent std entropy sources folded with
    // the counter, then split into two u64 halves.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);

    // RandomState carries a per-process random seed; hashing distinct inputs
    // through two fresh hashers pulls two different mixed 64-bit words out of it.
    let mut h1 = RandomState::new().build_hasher();
    h1.write_u64(nanos);
    h1.write_u64(count);
    let hi = h1.finish();

    let mut h2 = RandomState::new().build_hasher();
    h2.write_u64(count);
    h2.write_u64(nanos.rotate_left(32));
    let lo = h2.finish() ^ nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15);

    // Lay the 128 bits out as eight 16-bit groups, then stamp the version
    // nibble (4) and the variant nibble (8..b) into the canonical positions.
    let b = [
        (hi >> 48) as u16,
        (hi >> 32) as u16,
        (hi >> 16) as u16,
        hi as u16,
        (lo >> 48) as u16,
        (lo >> 32) as u16,
        (lo >> 16) as u16,
        lo as u16,
    ];
    // time_hi_and_version: high nibble forced to 0b0100 (version 4).
    let ver = (b[3] & 0x0FFF) | 0x4000;
    // clock_seq_hi_and_reserved: top two bits forced to 0b10 (RFC 4122 variant),
    // giving a `y` nibble in {8,9,a,b}.
    let var = (b[4] & 0x3FFF) | 0x8000;

    format!(
        "{:04x}{:04x}-{:04x}-{:04x}-{:04x}-{:04x}{:04x}{:04x}",
        b[0], b[1], b[2], ver, var, b[5], b[6], b[7]
    )
}

/// Mint an unguessable **capability token**: 32 bytes of OS entropy, hex-encoded
/// to a 64-char string. Unlike [`v4`], this **is a secret** — it is the per-pane
/// `AMUX_TOKEN` the ctl server matches to authenticate a request, so it must be
/// unpredictable, not merely unique.
///
/// Entropy comes straight from the OS CSPRNG (`BCryptGenRandom` on Windows,
/// `/dev/urandom` on unix) — no third-party crate. If the OS RNG is somehow
/// unavailable it falls back to folding several [`v4`] identifiers (weaker, but a
/// spawn must never fail for lack of a token); callers treat the token as a secret
/// regardless. The fallback path is not expected to run on a healthy system.
pub fn token() -> String {
    let mut bytes = [0u8; 32];
    if os_random(&mut bytes) {
        let mut s = String::with_capacity(64);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    } else {
        // Fallback only: OS entropy failed. Fold four non-crypto ids into 128 hex
        // chars of best-effort unpredictability rather than refuse to spawn.
        format!("{}{}{}{}", v4(), v4(), v4(), v4()).replace('-', "")
    }
}

/// Fill `buf` with cryptographically-strong bytes from the OS. Returns `false`
/// (leaving `buf` unchanged) if the OS RNG could not be read.
#[cfg(windows)]
fn os_random(buf: &mut [u8]) -> bool {
    #[link(name = "bcrypt")]
    extern "system" {
        fn BCryptGenRandom(
            alg: *mut std::ffi::c_void,
            buf: *mut u8,
            len: u32,
            flags: u32,
        ) -> i32;
    }
    // Use the system-preferred RNG so no algorithm handle is needed.
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
    // Returns STATUS_SUCCESS (0) on success.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    status == 0
}

#[cfg(unix)]
fn os_random(buf: &mut [u8]) -> bool {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .is_ok()
}

#[cfg(not(any(windows, unix)))]
fn os_random(_buf: &mut [u8]) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn v4_has_canonical_shape_and_marker_nibbles() {
        let id = v4();
        // 8-4-4-4-12 hex groups.
        let groups: Vec<&str> = id.split('-').collect();
        assert_eq!(groups.len(), 5, "{id}");
        assert_eq!(groups[0].len(), 8, "{id}");
        assert_eq!(groups[1].len(), 4, "{id}");
        assert_eq!(groups[2].len(), 4, "{id}");
        assert_eq!(groups[3].len(), 4, "{id}");
        assert_eq!(groups[4].len(), 12, "{id}");
        // All hex.
        assert!(
            id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()),
            "{id}"
        );
        // Version nibble is 4.
        assert_eq!(groups[2].chars().next().unwrap(), '4', "version: {id}");
        // Variant nibble is one of 8,9,a,b.
        let y = groups[3].chars().next().unwrap();
        assert!(matches!(y, '8' | '9' | 'a' | 'b'), "variant: {id}");
    }

    #[test]
    fn ten_thousand_ids_are_all_distinct() {
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            assert!(seen.insert(v4()), "duplicate uuid minted");
        }
    }

    #[test]
    fn token_is_64_hex_chars_from_os_entropy() {
        let t = token();
        // On any supported platform the OS RNG path is taken → 32 bytes → 64 hex.
        // (The v4 fallback would be 128 chars; assert we are NOT on it here.)
        assert_eq!(t.len(), 64, "expected 32 bytes of OS entropy hex-encoded: {t}");
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()), "not hex: {t}");
    }

    #[test]
    fn tokens_are_distinct_and_unlike_uuids() {
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            let t = token();
            assert!(seen.insert(t.clone()), "duplicate token minted");
            // A token is a raw hex secret, not a dashed uuid.
            assert!(!t.contains('-'), "token must not look like a uuid: {t}");
        }
    }

    #[test]
    fn os_random_fills_all_bytes() {
        // Two independent draws must differ in overwhelming probability, proving
        // the buffer is actually written (not left zeroed) by the OS RNG.
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        assert!(os_random(&mut a), "OS RNG must succeed on this platform");
        assert!(os_random(&mut b), "OS RNG must succeed on this platform");
        assert_ne!(a, b, "two OS-random draws must differ");
        assert_ne!(a, [0u8; 32], "OS RNG must not leave the buffer zeroed");
    }
}
