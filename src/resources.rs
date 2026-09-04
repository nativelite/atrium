//! Host-derived limits so a fleet can't exhaust the machine.
//!
//! The binding constraint on how many agents a box can run is **not** amux — a
//! pane costs one PTY, one emulator buffer, and one bounded tailer — but the
//! *agent processes*, each a heavyweight language-model runtime (hundreds of MB).
//! So the pane cap is derived from host RAM: how many agents fit with headroom.
//! The pure [`pane_cap`] does the arithmetic (unit-tested without touching the
//! OS); the thin wrappers gather the real numbers and read the operator overrides.

/// Fraction of total RAM we're willing to budget for agents (the rest is the OS,
/// the terminal, the operator's other work). Deliberately conservative.
const RAM_FRACTION: f64 = 0.75;
/// Default assumed resident footprint of one agent CLI process. A Node/Python
/// agent runtime is commonly a few hundred MB; 768 MiB leaves margin. Tunable via
/// `AMUX_AGENT_MB`.
const DEFAULT_AGENT_MB: u64 = 768;
/// Portable fallback when total RAM can't be detected: agents are mostly idle
/// (waiting on an API), so several per core is reasonable.
const AGENTS_PER_CORE: usize = 4;
/// A floor so a detection quirk (tiny reported RAM) can never cap the fleet to
/// zero and wedge spawning entirely.
const MIN_CAP: usize = 2;

/// Environment override for the absolute pane cap (`AMUX_MAX_PANES`). When set and
/// parseable, it wins outright — the operator knows their box.
pub const ENV_MAX_PANES: &str = "AMUX_MAX_PANES";
/// Environment override for the per-agent RAM estimate in MiB (`AMUX_AGENT_MB`).
pub const ENV_AGENT_MB: &str = "AMUX_AGENT_MB";

/// The maximum number of concurrent panes, from first available of: an explicit
/// `override_`; a RAM budget (`ram_bytes × RAM_FRACTION / per_agent_bytes`); or a
/// per-core fallback when RAM is unknown. Never below [`MIN_CAP`].
pub fn pane_cap(
    override_: Option<usize>,
    ram_bytes: Option<u64>,
    cores: usize,
    per_agent_bytes: u64,
) -> usize {
    if let Some(n) = override_ {
        return n.max(1);
    }
    match ram_bytes {
        Some(r) => {
            let usable = (r as f64 * RAM_FRACTION) as u64;
            ((usable / per_agent_bytes.max(1)) as usize).max(MIN_CAP)
        }
        None => (cores.max(1) * AGENTS_PER_CORE).max(MIN_CAP),
    }
}

/// The effective cap for this host, honoring `AMUX_MAX_PANES` / `AMUX_AGENT_MB`.
pub fn effective_cap() -> usize {
    let override_ = std::env::var(ENV_MAX_PANES)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok());
    let per_agent_mb = std::env::var(ENV_AGENT_MB)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&mb| mb > 0)
        .unwrap_or(DEFAULT_AGENT_MB);
    pane_cap(
        override_,
        total_ram_bytes(),
        cores(),
        per_agent_mb * 1024 * 1024,
    )
}

/// Logical CPU count (std, portable); `1` if it can't be determined.
fn cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Total physical RAM in bytes, best-effort. `None` ⇒ callers fall back to a
/// per-core cap. Detected on Windows and Linux; other platforms return `None`.
#[cfg(windows)]
fn total_ram_bytes() -> Option<u64> {
    #[repr(C)]
    struct MemoryStatusEx {
        dw_length: u32,
        dw_memory_load: u32,
        ull_total_phys: u64,
        ull_avail_phys: u64,
        ull_total_page_file: u64,
        ull_avail_page_file: u64,
        ull_total_virtual: u64,
        ull_avail_virtual: u64,
        ull_avail_extended_virtual: u64,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }
    let mut m = MemoryStatusEx {
        dw_length: std::mem::size_of::<MemoryStatusEx>() as u32,
        dw_memory_load: 0,
        ull_total_phys: 0,
        ull_avail_phys: 0,
        ull_total_page_file: 0,
        ull_avail_page_file: 0,
        ull_total_virtual: 0,
        ull_avail_virtual: 0,
        ull_avail_extended_virtual: 0,
    };
    // SAFETY: `m` is a valid, fully-initialized MEMORYSTATUSEX with `dw_length`
    // set to its own size, exactly as the Win32 contract requires.
    let ok = unsafe { GlobalMemoryStatusEx(&mut m) };
    (ok != 0).then_some(m.ull_total_phys)
}

#[cfg(target_os = "linux")]
fn total_ram_bytes() -> Option<u64> {
    // /proc/meminfo: a `MemTotal:   16384000 kB` line.
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

#[cfg(not(any(windows, target_os = "linux")))]
fn total_ram_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    const GB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn explicit_override_wins() {
        assert_eq!(pane_cap(Some(20), Some(64 * GB), 8, 768 << 20), 20);
        // …but never below one.
        assert_eq!(pane_cap(Some(0), None, 8, 768 << 20), 1);
    }

    #[test]
    fn ram_budget_drives_the_default() {
        // 16 GiB × 0.75 / 0.75 GiB ≈ 16 agents.
        let cap = pane_cap(None, Some(16 * GB), 4, 768 * 1024 * 1024);
        assert_eq!(cap, 16);
        // A bigger box allows more; a laptop allows fewer.
        assert!(pane_cap(None, Some(64 * GB), 8, 768 * 1024 * 1024) > cap);
        assert!(pane_cap(None, Some(8 * GB), 4, 768 * 1024 * 1024) < cap);
    }

    #[test]
    fn falls_back_to_cores_when_ram_unknown() {
        assert_eq!(pane_cap(None, None, 8, 768 << 20), 8 * AGENTS_PER_CORE);
        assert_eq!(
            pane_cap(None, None, 0, 768 << 20),
            MIN_CAP.max(AGENTS_PER_CORE)
        );
    }

    #[test]
    fn never_caps_below_the_floor() {
        // A pathologically small reported RAM still leaves room to run a couple.
        assert_eq!(
            pane_cap(None, Some(100 * 1024 * 1024), 4, 768 * 1024 * 1024),
            MIN_CAP
        );
    }
}
