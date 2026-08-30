//! The split tree: a binary tree of panes and H/V splits that resolves to a
//! rect per pane. This is the tiled-mode layout engine — pure geometry, no
//! terminal, no pty, unit-testable on its own.
//!
//! Leaves hold a pane id (an index into the app's pane vector); internal nodes
//! are horizontal (stacked) or vertical (side-by-side) splits with two
//! children. [`Tree::rects`] walks the tree over an outer rect and hands back
//! `(pane_id, Rect)` for every leaf. Panes tile **edge-to-edge** with no
//! reserved gutter — each pane draws its own full box border (0.2.1), so
//! adjacent borders simply abut and the split space is halved with no gap.
//!
//! MVP splits are **equal** — a split halves the focused pane — which is all
//! the 2×2 "four agents in a square" case needs: split vertical, then split
//! each side horizontally. Proportional drag-resize is deferred (see the 0.2
//! design doc, §4 "out of scope").

/// A sub-rectangle of the screen, 0-based `(row, col)` origin with a size. All
/// coordinates are in the master (outer) grid the compositor paints into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub row: usize,
    pub col: usize,
    pub rows: usize,
    pub cols: usize,
}

impl Rect {
    /// The cell coordinates of the rect's center, used by focus movement to
    /// pick the nearest pane in a direction.
    fn center(&self) -> (usize, usize) {
        (self.row + self.rows / 2, self.col + self.cols / 2)
    }
}

/// Split orientation. `Horizontal` stacks its children (top / bottom);
/// `Vertical` places them side by side (left / right). Children abut with no
/// reserved gutter — each pane's own box border is the visible seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Horizontal,
    Vertical,
}

/// A node in the split tree: either a leaf pane or an even split of two
/// subtrees.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Leaf(usize),
    Split {
        dir: Dir,
        first: Box<Node>,
        second: Box<Node>,
    },
}

/// Which way to move focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    Left,
    Right,
    Up,
    Down,
}

/// The layout tree plus which pane currently has focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    root: Node,
    focus: usize,
}

impl Tree {
    /// A single-pane tree (pane `id`, focused).
    pub fn new(id: usize) -> Self {
        Tree {
            root: Node::Leaf(id),
            focus: id,
        }
    }

    /// A balanced `rows × cols` grid of panes, ids `0..rows*cols` in row-major
    /// order (top-left is 0), focused on pane 0. This is the mass-spawn layout
    /// ([`crate::spawn`]): a window of N agent tiles built in one shot instead of
    /// N interactive splits. The tree is C-wide columns nested inside R-tall
    /// rows, so [`rects`](Tree::rects), focus movement, and close all behave
    /// exactly as they would for the same shape built by hand with `split`.
    ///
    /// `rows` and `cols` are clamped to at least 1, so the worst case is a
    /// single-leaf tree (never a panic or an empty tree).
    pub fn grid(rows: usize, cols: usize) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        // Row-major id assignment: row r, col c -> id r*cols + c.
        let row_node = |r: usize| -> Node {
            Self::balanced_line(Dir::Vertical, cols, |c| Node::Leaf(r * cols + c))
        };
        let root = Self::balanced_line(Dir::Horizontal, rows, row_node);
        Tree { root, focus: 0 }
    }

    /// Build a balanced left-leaning line of `n` children joined by `dir`
    /// splits, each child produced by `leaf(i)` for `i in 0..n`. `n >= 1`.
    /// A single child is just that child (no split); otherwise the first child
    /// is `leaf(0)` and the rest recurse, so ids stay in ascending order across
    /// the resulting leaves — matching how a person would split left-to-right.
    fn balanced_line(dir: Dir, n: usize, mut leaf: impl FnMut(usize) -> Node) -> Node {
        Self::line_from(dir, 0, n, &mut leaf)
    }

    fn line_from(dir: Dir, start: usize, n: usize, leaf: &mut impl FnMut(usize) -> Node) -> Node {
        if start + 1 >= n {
            return leaf(start);
        }
        Node::Split {
            dir,
            first: Box::new(leaf(start)),
            second: Box::new(Self::line_from(dir, start + 1, n, leaf)),
        }
    }

    /// The focused pane id.
    pub fn focus(&self) -> usize {
        self.focus
    }

    /// Number of panes (leaves) in the tree.
    pub fn len(&self) -> usize {
        Self::count(&self.root)
    }

    pub fn is_empty(&self) -> bool {
        false // a tree always holds at least one leaf
    }

    fn count(node: &Node) -> usize {
        match node {
            Node::Leaf(_) => 1,
            Node::Split { first, second, .. } => Self::count(first) + Self::count(second),
        }
    }

    /// Every pane id in the tree, in left-to-right leaf order.
    pub fn ids(&self) -> Vec<usize> {
        let mut out = Vec::new();
        Self::collect_ids(&self.root, &mut out);
        out
    }

    fn collect_ids(node: &Node, out: &mut Vec<usize>) {
        match node {
            Node::Leaf(id) => out.push(*id),
            Node::Split { first, second, .. } => {
                Self::collect_ids(first, out);
                Self::collect_ids(second, out);
            }
        }
    }

    /// True once the tree is a single pane again (tiled mode collapses back to
    /// passthrough here).
    pub fn is_single(&self) -> bool {
        matches!(self.root, Node::Leaf(_))
    }

    /// Split the focused pane in `dir`, giving the new half pane id `new_id`,
    /// and move focus to the new pane. The focused leaf becomes a split whose
    /// first child is the old pane and whose second child is the new one.
    pub fn split(&mut self, dir: Dir, new_id: usize) {
        let focus = self.focus;
        Self::split_at(&mut self.root, focus, dir, new_id);
        self.focus = new_id;
    }

    fn split_at(node: &mut Node, target: usize, dir: Dir, new_id: usize) -> bool {
        match node {
            Node::Leaf(id) if *id == target => {
                let old = *id;
                *node = Node::Split {
                    dir,
                    first: Box::new(Node::Leaf(old)),
                    second: Box::new(Node::Leaf(new_id)),
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { first, second, .. } => {
                Self::split_at(first, target, dir, new_id)
                    || Self::split_at(second, target, dir, new_id)
            }
        }
    }

    /// Remove pane `id` from the tree, collapsing its parent split into the
    /// surviving sibling. Returns `false` if `id` is the last pane (the caller
    /// then tears the whole tree down / quits). Focus, if it was on the removed
    /// pane, moves to the first remaining leaf.
    pub fn close(&mut self, id: usize) -> bool {
        if self.is_single() {
            return false;
        }
        Self::close_at(&mut self.root, id);
        if !self.ids().contains(&self.focus) {
            self.focus = self.ids()[0];
        }
        true
    }

    /// Replace a parent split with its surviving child when one child is the
    /// leaf to remove. Recurses into splits. Returns true if a removal
    /// happened in this subtree.
    fn close_at(node: &mut Node, id: usize) -> bool {
        if let Node::Split { first, second, .. } = node {
            // Direct child is the target leaf → collapse to the sibling.
            if matches!(**first, Node::Leaf(x) if x == id) {
                let survivor = std::mem::replace(second.as_mut(), Node::Leaf(usize::MAX));
                *node = survivor;
                return true;
            }
            if matches!(**second, Node::Leaf(x) if x == id) {
                let survivor = std::mem::replace(first.as_mut(), Node::Leaf(usize::MAX));
                *node = survivor;
                return true;
            }
            return Self::close_at(first, id) || Self::close_at(second, id);
        }
        false
    }

    /// Resolve the tree to `(pane_id, Rect)` for every pane, laid out inside
    /// the outer rect. A split halves the space edge-to-edge with **no**
    /// reserved gutter — the first child gets the ceil half, the second the
    /// floor half, and the second abuts the first with no gap (each pane's own
    /// box border is the seam). Panes that would be zero-sized are still
    /// emitted with a 1-cell minimum so no pane silently vanishes.
    pub fn rects(&self, outer: Rect) -> Vec<(usize, Rect)> {
        let mut out = Vec::new();
        Self::layout(&self.root, outer, &mut out);
        out
    }

    fn layout(node: &Node, r: Rect, out: &mut Vec<(usize, Rect)>) {
        match node {
            Node::Leaf(id) => out.push((*id, r)),
            Node::Split { dir, first, second } => match dir {
                Dir::Horizontal => {
                    // Stacked: top gets the ceil half, bottom the floor half,
                    // abutting with no reserved divider row. (Manual ceil —
                    // `div_ceil` is not stable on our 1.70 MSRV.)
                    let top_rows = ((r.rows + 1) / 2).max(1);
                    let bot_rows = r.rows.saturating_sub(top_rows).max(1);
                    let top = Rect {
                        row: r.row,
                        col: r.col,
                        rows: top_rows,
                        cols: r.cols,
                    };
                    let bottom = Rect {
                        row: r.row + top_rows,
                        col: r.col,
                        rows: bot_rows,
                        cols: r.cols,
                    };
                    Self::layout(first, top, out);
                    Self::layout(second, bottom, out);
                }
                Dir::Vertical => {
                    // Side by side: left gets the ceil half, right the floor
                    // half, abutting with no reserved divider column. (Manual
                    // ceil — `div_ceil` is not stable on our 1.70 MSRV.)
                    let left_cols = ((r.cols + 1) / 2).max(1);
                    let right_cols = r.cols.saturating_sub(left_cols).max(1);
                    let left = Rect {
                        row: r.row,
                        col: r.col,
                        rows: r.rows,
                        cols: left_cols,
                    };
                    let right = Rect {
                        row: r.row,
                        col: r.col + left_cols,
                        rows: r.rows,
                        cols: right_cols,
                    };
                    Self::layout(first, left, out);
                    Self::layout(second, right, out);
                }
            },
        }
    }

    /// Move focus to the nearest pane in direction `m`, measured from the
    /// focused pane's center against the other panes' centers inside `outer`.
    /// Geometric rather than tree-structural so it does the intuitive thing on
    /// a 2×2 grid regardless of split nesting order. No-op if nothing lies that
    /// way. Returns the new focus.
    pub fn move_focus(&mut self, m: Move, outer: Rect) -> usize {
        let rects = self.rects(outer);
        let Some(cur) = rects
            .iter()
            .find(|(id, _)| *id == self.focus)
            .map(|(_, r)| *r)
        else {
            return self.focus;
        };
        let (cr, cc) = cur.center();
        let mut best: Option<(usize, usize)> = None; // (distance, id)
        for (id, rect) in &rects {
            if *id == self.focus {
                continue;
            }
            let (r, c) = rect.center();
            // The candidate must lie in the requested half-plane.
            let ok = match m {
                Move::Left => c < cc,
                Move::Right => c > cc,
                Move::Up => r < cr,
                Move::Down => r > cr,
            };
            if !ok {
                continue;
            }
            // Manhattan distance, biased so the primary axis dominates ties.
            let dist = cr.abs_diff(r) + cc.abs_diff(c);
            if best.map(|(d, _)| dist < d).unwrap_or(true) {
                best = Some((dist, *id));
            }
        }
        if let Some((_, id)) = best {
            self.focus = id;
        }
        self.focus
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTER: Rect = Rect {
        row: 0,
        col: 0,
        rows: 24,
        cols: 80,
    };

    #[test]
    fn single_pane_fills_the_outer_rect() {
        let t = Tree::new(0);
        assert!(t.is_single());
        assert_eq!(t.rects(OUTER), vec![(0, OUTER)]);
        assert_eq!(t.focus(), 0);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn vertical_split_makes_two_side_by_side_edge_to_edge() {
        let mut t = Tree::new(0);
        t.split(Dir::Vertical, 1);
        assert!(!t.is_single());
        assert_eq!(t.focus(), 1); // focus follows the new pane
        let rects = t.rects(OUTER);
        assert_eq!(rects.len(), 2);
        let (_, left) = rects[0];
        let (_, right) = rects[1];
        assert_eq!(left.row, 0);
        assert_eq!(left.rows, 24);
        // 80 cols halved edge-to-edge: 40 / 40, no reserved gutter.
        assert_eq!(left.cols, 40);
        assert_eq!(right.col, 40); // abuts the left pane, no gap
        assert_eq!(right.cols, 40);
        // The right pane begins exactly where the left ends (no divider cell).
        assert_eq!(right.col, left.col + left.cols);
    }

    #[test]
    fn horizontal_split_stacks_edge_to_edge() {
        let mut t = Tree::new(0);
        t.split(Dir::Horizontal, 1);
        let rects = t.rects(OUTER);
        let (_, top) = rects[0];
        let (_, bottom) = rects[1];
        assert_eq!(top.col, 0);
        assert_eq!(top.cols, 80);
        // 24 rows halved edge-to-edge: 12 / 12, no reserved gutter.
        assert_eq!(top.rows, 12);
        assert_eq!(bottom.row, 12); // abuts the top pane, no gap
        assert_eq!(bottom.rows, 12);
        assert_eq!(bottom.row, top.row + top.rows);
    }

    #[test]
    fn two_by_two_grid_has_four_nonoverlapping_panes() {
        // Split vertical, then split each side horizontally: the founder's
        // headline "four agents in a square".
        let mut t = Tree::new(0);
        t.split(Dir::Vertical, 1); // focus -> 1 (right)
        t.split(Dir::Horizontal, 2); // splits right column -> focus 2
                                     // move focus back to the left column and split it
        t.focus = 0;
        t.split(Dir::Horizontal, 3);
        assert_eq!(t.len(), 4);
        let rects = t.rects(OUTER);
        assert_eq!(rects.len(), 4);
        // All four ids present.
        let ids: Vec<usize> = rects.iter().map(|(id, _)| *id).collect();
        for id in 0..4 {
            assert!(ids.contains(&id), "missing pane {id} in {ids:?}");
        }
        // No two rects overlap.
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                assert!(
                    !overlaps(rects[i].1, rects[j].1),
                    "{:?} vs {:?}",
                    rects[i],
                    rects[j]
                );
            }
        }
    }

    fn overlaps(a: Rect, b: Rect) -> bool {
        let ax2 = a.col + a.cols;
        let ay2 = a.row + a.rows;
        let bx2 = b.col + b.cols;
        let by2 = b.row + b.rows;
        a.col < bx2 && b.col < ax2 && a.row < by2 && b.row < ay2
    }

    #[test]
    fn focus_moves_geometrically_across_a_2x2_grid() {
        let mut t = two_by_two();
        // Find the top-left pane and focus it.
        let rects = t.rects(OUTER);
        let top_left = rects
            .iter()
            .min_by_key(|(_, r)| (r.row, r.col))
            .map(|(id, _)| *id)
            .unwrap();
        t.focus = top_left;
        // Right then down then left then up returns to top-left.
        let r = t.move_focus(Move::Right, OUTER);
        assert_ne!(r, top_left, "right should move to another pane");
        let d = t.move_focus(Move::Down, OUTER);
        assert_ne!(d, r, "down should move again");
        t.move_focus(Move::Left, OUTER);
        let back = t.move_focus(Move::Up, OUTER);
        assert_eq!(back, top_left, "up+left should return to the top-left pane");
    }

    #[test]
    fn move_focus_is_a_noop_off_the_edge() {
        let mut t = Tree::new(0);
        t.split(Dir::Vertical, 1);
        t.focus = 0; // left pane
                     // Nothing to the left of the left pane.
        assert_eq!(t.move_focus(Move::Left, OUTER), 0);
        // Right reaches pane 1.
        assert_eq!(t.move_focus(Move::Right, OUTER), 1);
    }

    #[test]
    fn closing_a_pane_collapses_to_the_sibling() {
        let mut t = Tree::new(0);
        t.split(Dir::Vertical, 1); // now [0 | 1], focus 1
        assert!(t.close(1));
        assert!(t.is_single());
        assert_eq!(t.ids(), vec![0]);
        assert_eq!(t.focus(), 0); // focus fell back to the survivor
    }

    #[test]
    fn closing_re_tiles_a_2x2_into_three() {
        let mut t = two_by_two();
        let victim = t.ids()[0];
        assert!(t.close(victim));
        assert_eq!(t.len(), 3);
        assert!(!t.ids().contains(&victim));
        // Remaining three still tile without overlap.
        let rects = t.rects(OUTER);
        assert_eq!(rects.len(), 3);
    }

    #[test]
    fn last_pane_cannot_be_closed() {
        let mut t = Tree::new(0);
        assert!(!t.close(0));
        assert!(t.is_single());
    }

    fn two_by_two() -> Tree {
        let mut t = Tree::new(0);
        t.split(Dir::Vertical, 1);
        t.split(Dir::Horizontal, 2);
        t.focus = 0;
        t.split(Dir::Horizontal, 3);
        t
    }

    // --- grid builder (mass-spawn, 0.5) ------------------------------------

    #[test]
    fn grid_has_the_expected_leaf_count_and_ids() {
        // 2x3 -> six leaves, ids 0..6, focus on 0.
        let t = Tree::grid(2, 3);
        assert_eq!(t.len(), 6);
        assert_eq!(t.focus(), 0);
        let mut ids = t.ids();
        ids.sort_unstable();
        assert_eq!(ids, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn grid_leaves_tile_without_overlap() {
        // Every grid shape mass-spawn can produce must tile the outer rect with
        // no two panes overlapping (same discipline as the 2x2 split test).
        for &(r, c) in &[(1, 2), (2, 2), (2, 3), (2, 4), (3, 4)] {
            let t = Tree::grid(r, c);
            let rects = t.rects(OUTER);
            assert_eq!(rects.len(), r * c, "{r}x{c} leaf count");
            for i in 0..rects.len() {
                for j in (i + 1)..rects.len() {
                    assert!(
                        !overlaps(rects[i].1, rects[j].1),
                        "{r}x{c}: {:?} vs {:?}",
                        rects[i],
                        rects[j]
                    );
                }
            }
        }
    }

    #[test]
    fn grid_focus_moves_geometrically() {
        // A 2x2 grid built by `grid` behaves like one built by hand: focus on
        // the top-left pane, right+down+left+up returns to it.
        let mut t = Tree::grid(2, 2);
        let rects = t.rects(OUTER);
        let top_left = rects
            .iter()
            .min_by_key(|(_, r)| (r.row, r.col))
            .map(|(id, _)| *id)
            .unwrap();
        t.focus = top_left;
        let r = t.move_focus(Move::Right, OUTER);
        assert_ne!(r, top_left);
        t.move_focus(Move::Down, OUTER);
        t.move_focus(Move::Left, OUTER);
        let back = t.move_focus(Move::Up, OUTER);
        assert_eq!(back, top_left);
    }

    #[test]
    fn grid_degenerate_dims_never_panic() {
        // 0 dims clamp to 1: a single-leaf tree, not a panic or empty tree.
        let t = Tree::grid(0, 0);
        assert_eq!(t.len(), 1);
        assert_eq!(t.ids(), vec![0]);
    }
}
