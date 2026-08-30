//! Mass-spawn: open **N agent panes at once** in one balanced-grid window.
//!
//! amux's own `-n <N>` / `--grid <R>x<C>` flags are stripped off the front —
//! after `--identity` (see [`crate::identity::parse`]), before the hosted
//! command begins — exactly the way the identity flag is. The first non-flag
//! token starts the child; a later `-n` that belongs to the hosted program is
//! never eaten. The grammar is
//! `amux [--identity <name>] [-n <N> | --grid <R>x<C>] <command...>`.
//!
//! This module owns the two *pure* seams so the wiring is testable without a
//! pty or a layout tree:
//!
//! * [`parse`] pulls the count/grid flag out of the argument vector, returning
//!   the requested [`Grid`] (if any) and the remaining command, or a clear
//!   error string for a bad value.
//! * [`Grid::balanced`] turns a plain `-n <N>` into a balanced `R×C` shape:
//!   rows = the factor of N closest to `sqrt(N)` (biased to keep it even-ish),
//!   cols = N / rows, so `4→2×2`, `6→2×3`, `8→2×4`.
//!
//! Construction of the N-leaf split tree from a [`Grid`] lives in
//! [`crate::layout::Tree::grid`]; the run loop spawns one pane per leaf, each
//! its own session, all under the same identity.

/// A requested grid of agent panes. `rows * cols == total`; both dimensions are
/// at least 1 and the product is at least 2 (mass-spawn is never a single pane —
/// that is the unchanged default path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grid {
    pub rows: usize,
    pub cols: usize,
}

impl Grid {
    /// Total panes in the grid.
    pub fn total(&self) -> usize {
        self.rows * self.cols
    }

    /// A balanced `R×C` shape for a plain count `n`: rows = the divisor of `n`
    /// closest to `sqrt(n)`, cols = `n / rows`. For the even counts mass-spawn
    /// accepts this yields the intuitive squares/rectangles: `4→2×2`, `6→2×3`,
    /// `8→2×4`, `12→3×4`. Rows are the smaller-or-equal dimension so the grid is
    /// never taller than it is wide.
    pub fn balanced(n: usize) -> Grid {
        // Largest divisor of `n` that is <= sqrt(n): the most balanced factor
        // pair. `rows <= cols` always, so tiles stay wider than tall.
        let mut rows = 1usize;
        let mut d = 1usize;
        while d * d <= n {
            if n % d == 0 {
                rows = d;
            }
            d += 1;
        }
        Grid {
            rows,
            cols: n / rows,
        }
    }
}

/// Extract amux's own `-n <N>` / `--grid <R>x<C>` flag from the front-loaded
/// argument vector (which [`crate::identity::parse`] has already stripped of
/// `--identity`). Returns the requested [`Grid`] (if any) and the remaining
/// vector — the hosted command and its args, untouched — or an `Err(msg)` with
/// a clear reason for a bad value.
///
/// Like the identity flag, the count/grid flag is amux's, so it is recognized
/// only as a *leading* option before the hosted command begins. The first
/// non-flag token starts the command; everything from there is the child's, so
/// a later `-n` the hosted program takes is never eaten.
///
/// Rules:
/// * `-n <N>` — `N` a positive **multiple of 2** (2, 4, 6, …); an odd or zero
///   `N`, or a non-numeric one, is rejected.
/// * `--grid <R>x<C>` — both `R` and `C` at least 1 and `R*C` at least 2 (a
///   1×1 grid is a single pane, which is the default path, not mass-spawn).
/// * Neither flag → `(None, command)`: today's single-pane behavior, unchanged.
/// * The flag given more than once: the last occurrence wins (matching the
///   identity flag's rule), so a later `-n`/`--grid` overrides an earlier one.
pub fn parse(args: &[String]) -> Result<(Option<Grid>, Vec<String>), String> {
    let mut grid: Option<Grid> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| "-n needs a value (a positive multiple of 2)".to_string())?;
                grid = Some(parse_count(val)?);
                i += 2;
            }
            "--grid" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| "--grid needs a value like 2x3".to_string())?;
                grid = Some(parse_grid(val)?);
                i += 2;
            }
            s if s.starts_with("-n=") => {
                grid = Some(parse_count(&s["-n=".len()..])?);
                i += 1;
            }
            s if s.starts_with("--grid=") => {
                grid = Some(parse_grid(&s["--grid=".len()..])?);
                i += 1;
            }
            // First non-flag token: the hosted command starts here. Stop parsing
            // amux options so the child owns the rest verbatim.
            _ => return Ok((grid, args[i..].to_vec())),
        }
    }
    Ok((grid, Vec::new()))
}

/// Parse the `-n <N>` value: a positive multiple of 2.
fn parse_count(val: &str) -> Result<Grid, String> {
    let n: usize = val
        .parse()
        .map_err(|_| "-n must be a positive multiple of 2".to_string())?;
    if n == 0 || n % 2 != 0 {
        return Err("-n must be a positive multiple of 2".to_string());
    }
    Ok(Grid::balanced(n))
}

/// Parse the `--grid <R>x<C>` value: two positive dimensions whose product is
/// at least 2.
fn parse_grid(val: &str) -> Result<Grid, String> {
    let bad = || "--grid must look like 2x3 (rows x cols, product >= 2)".to_string();
    // Accept `x` or `X` as the separator.
    let sep = val.find(['x', 'X']).ok_or_else(bad)?;
    let rows: usize = val[..sep].parse().map_err(|_| bad())?;
    let cols: usize = val[sep + 1..].parse().map_err(|_| bad())?;
    if rows < 1 || cols < 1 || rows * cols < 2 {
        return Err(bad());
    }
    Ok(Grid { rows, cols })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn neither_flag_is_single_pane() {
        let (grid, cmd) = parse(&v(&["claude", "--continue"])).unwrap();
        assert_eq!(grid, None);
        assert_eq!(cmd, v(&["claude", "--continue"]));
    }

    #[test]
    fn dash_n_four_is_four_panes() {
        let (grid, cmd) = parse(&v(&["-n", "4", "claude"])).unwrap();
        assert_eq!(grid.map(|g| g.total()), Some(4));
        assert_eq!(cmd, v(&["claude"]));
    }

    #[test]
    fn dash_n_six_is_six_panes() {
        let (grid, _) = parse(&v(&["-n", "6", "claude"])).unwrap();
        assert_eq!(grid.map(|g| g.total()), Some(6));
    }

    #[test]
    fn dash_n_odd_is_rejected() {
        let err = parse(&v(&["-n", "3", "claude"])).unwrap_err();
        assert_eq!(err, "-n must be a positive multiple of 2");
    }

    #[test]
    fn dash_n_zero_is_rejected() {
        let err = parse(&v(&["-n", "0", "claude"])).unwrap_err();
        assert_eq!(err, "-n must be a positive multiple of 2");
    }

    #[test]
    fn dash_n_non_numeric_is_rejected() {
        let err = parse(&v(&["-n", "lots", "claude"])).unwrap_err();
        assert_eq!(err, "-n must be a positive multiple of 2");
    }

    #[test]
    fn grid_two_by_three_is_six_panes() {
        let (grid, cmd) = parse(&v(&["--grid", "2x3", "claude"])).unwrap();
        assert_eq!(grid, Some(Grid { rows: 2, cols: 3 }));
        assert_eq!(grid.map(|g| g.total()), Some(6));
        assert_eq!(cmd, v(&["claude"]));
    }

    #[test]
    fn grid_accepts_capital_x() {
        let (grid, _) = parse(&v(&["--grid", "3X2", "claude"])).unwrap();
        assert_eq!(grid, Some(Grid { rows: 3, cols: 2 }));
    }

    #[test]
    fn grid_one_by_one_is_rejected() {
        let err = parse(&v(&["--grid", "1x1", "claude"])).unwrap_err();
        assert!(err.contains("--grid"), "{err}");
    }

    #[test]
    fn grid_malformed_is_rejected() {
        assert!(parse(&v(&["--grid", "abc", "claude"])).is_err());
        assert!(parse(&v(&["--grid", "2", "claude"])).is_err());
        assert!(parse(&v(&["--grid", "2x", "claude"])).is_err());
    }

    #[test]
    fn glued_forms_are_accepted() {
        let (grid, cmd) = parse(&v(&["-n=4", "claude"])).unwrap();
        assert_eq!(grid.map(|g| g.total()), Some(4));
        assert_eq!(cmd, v(&["claude"]));
        let (grid, _) = parse(&v(&["--grid=2x3", "claude"])).unwrap();
        assert_eq!(grid, Some(Grid { rows: 2, cols: 3 }));
    }

    #[test]
    fn a_flag_that_belongs_to_the_hosted_command_is_not_eaten() {
        // The command starts at the first non-flag token (`claude`); a `-n` the
        // hosted program takes lives after that and must pass through.
        let (grid, cmd) = parse(&v(&["claude", "-n", "5"])).unwrap();
        assert_eq!(grid, None);
        assert_eq!(cmd, v(&["claude", "-n", "5"]));
    }

    #[test]
    fn missing_value_is_a_clear_error() {
        assert!(parse(&v(&["-n"])).is_err());
        assert!(parse(&v(&["--grid"])).is_err());
    }

    #[test]
    fn last_flag_wins() {
        let (grid, _) = parse(&v(&["-n", "2", "-n", "4", "claude"])).unwrap();
        assert_eq!(grid.map(|g| g.total()), Some(4));
    }

    #[test]
    fn balanced_grid_shapes_are_sensible() {
        // Product must equal N, and rows <= cols (never taller than wide).
        for &(n, r, c) in &[(2, 1, 2), (4, 2, 2), (6, 2, 3), (8, 2, 4), (12, 3, 4)] {
            let g = Grid::balanced(n);
            assert_eq!(g.total(), n, "product == N for {n}");
            assert_eq!((g.rows, g.cols), (r, c), "shape for {n}");
            assert!(g.rows <= g.cols, "rows <= cols for {n}");
        }
    }
}
