//! Tree -> window rectangles. One pass, O(nodes), no allocation beyond the
//! caller's reusable output vector.

use tatami_protocol::{Axis, Rect};

use crate::ids::{NodeId, WindowId};
use crate::tree::{Leaf, NodeKind, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Insets {
    pub top: f64,
    pub bottom: f64,
    pub left: f64,
    pub right: f64,
}

impl Insets {
    pub fn uniform(v: f64) -> Self {
        Self {
            top: v,
            bottom: v,
            left: v,
            right: v,
        }
    }
    pub fn apply(&self, r: Rect) -> Rect {
        r.inset(self.left, self.top, self.right, self.bottom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Gaps {
    pub inner: f64,
    pub outer: Insets,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub window: WindowId,
    pub rect: Rect,
    /// Stacking within the workspace: 0 is frontmost. Only accordions
    /// overlap, so tiles share their container's value.
    pub z: u16,
}

struct Ctx {
    inner: f64,
    focused: Option<NodeId>,
    pad: f64,
}

fn span(r: &Rect, axis: Axis) -> (f64, f64) {
    match axis {
        Axis::H => (r.x, r.w),
        Axis::V => (r.y, r.h),
    }
}

fn with_span(r: Rect, axis: Axis, start: f64, len: f64) -> Rect {
    match axis {
        Axis::H => Rect {
            x: start,
            w: len,
            ..r
        },
        Axis::V => Rect {
            y: start,
            h: len,
            ..r
        },
    }
}

/// Offset of the boundary after `cum` of `total` weight. Interior
/// boundaries round to whole points; the last one is exact, so the
/// children always sum to `avail` with no stray pixel.
fn boundary(avail: f64, cum: f64, total: f64, is_end: bool) -> f64 {
    if is_end {
        avail
    } else {
        (avail * cum / total).round()
    }
}

fn weight(tree: &Tree, id: NodeId) -> f64 {
    f64::from(tree.node(id).weight).max(1e-3)
}

/// AeroSpace's accordion offsets: the raised child leaves `pad` on each side
/// that has neighbours, and its immediate neighbours shift outward so a
/// sliver of them shows there.
fn accordion_insets(i: usize, n: usize, raised: usize, pad: f64) -> (f64, f64) {
    if n == 1 {
        (0.0, 0.0)
    } else if i == 0 {
        (0.0, pad)
    } else if i == n - 1 {
        (pad, 0.0)
    } else if i + 1 == raised {
        (0.0, 2.0 * pad)
    } else if i == raised + 1 {
        (2.0 * pad, 0.0)
    } else {
        (pad, pad)
    }
}

/// Child of `container` to raise: the one on the path to `focused`, else
/// the container's most recent, else the first.
fn raised_index(
    tree: &Tree,
    container: NodeId,
    children: &[NodeId],
    focused: Option<NodeId>,
) -> usize {
    let mut cur = focused;
    while let Some(c) = cur {
        let p = tree.parent(c);
        if p == Some(container) {
            return children.iter().position(|&k| k == c).unwrap_or(0);
        }
        cur = p;
    }
    tree.mru_of(container)
        .and_then(|m| children.iter().position(|&k| k == m))
        .unwrap_or(0)
}

/// Lay out the tree under `root` into `area` (the monitor's visible frame),
/// appending one placement per window. Pending leaves take space but emit
/// nothing.
pub fn compute(
    tree: &Tree,
    root: NodeId,
    area: Rect,
    gaps: &Gaps,
    focused: Option<NodeId>,
    accordion_padding: f64,
    out: &mut Vec<Placement>,
) {
    if !tree.contains(root) {
        return;
    }
    let ctx = Ctx {
        inner: gaps.inner,
        focused,
        pad: accordion_padding,
    };
    place(tree, root, gaps.outer.apply(area), 0, &ctx, out);
}

fn place(tree: &Tree, id: NodeId, rect: Rect, z: u16, ctx: &Ctx, out: &mut Vec<Placement>) {
    match &tree.node(id).kind {
        NodeKind::Leaf(Leaf::Window(w)) => out.push(Placement {
            window: *w,
            rect,
            z,
        }),
        NodeKind::Leaf(Leaf::Pending(_)) => {}
        NodeKind::Container {
            layout, children, ..
        } => {
            let n = children.len();
            if n == 0 {
                return;
            }
            let axis = layout.axis();
            let (start, len) = span(&rect, axis);
            if layout.is_accordion() {
                let raised = raised_index(tree, id, children, ctx.focused);
                for (i, &c) in children.iter().enumerate() {
                    let (lead, trail) = accordion_insets(i, n, raised, ctx.pad);
                    let r = with_span(rect, axis, start + lead, (len - lead - trail).max(0.0));
                    let dz = if i == raised {
                        0
                    } else {
                        1 + i.abs_diff(raised)
                    };
                    let dz = u16::try_from(dz).unwrap_or(u16::MAX);
                    place(tree, c, r, z.saturating_add(dz), ctx, out);
                }
            } else {
                let avail = (len - ctx.inner * (n - 1) as f64).max(0.0);
                let total: f64 = children.iter().map(|&c| weight(tree, c)).sum();
                let mut cum = 0.0;
                let mut prev = 0.0;
                for (i, &c) in children.iter().enumerate() {
                    cum += weight(tree, c);
                    let next = boundary(avail, cum, total, i + 1 == n);
                    let pos = start + prev + i as f64 * ctx.inner;
                    place(
                        tree,
                        c,
                        with_span(rect, axis, pos, next - prev),
                        z,
                        ctx,
                        out,
                    );
                    prev = next;
                }
            }
        }
    }
}

/// The rect `compute` gives `node` (container or leaf), without laying out
/// the rest of the tree: O(depth x siblings). Feeds `Tree::resize`.
pub fn node_rect(
    tree: &Tree,
    root: NodeId,
    area: Rect,
    gaps: &Gaps,
    focused: Option<NodeId>,
    accordion_padding: f64,
    node: NodeId,
) -> Option<Rect> {
    if node == root {
        return tree.contains(root).then(|| gaps.outer.apply(area));
    }
    let (p, i) = tree.index_in_parent(node)?;
    let pr = node_rect(tree, root, area, gaps, focused, accordion_padding, p)?;
    let layout = tree.layout_of(p);
    let kids = tree.children(p);
    let n = kids.len();
    let axis = layout.axis();
    let (start, len) = span(&pr, axis);
    if layout.is_accordion() {
        let raised = raised_index(tree, p, kids, focused);
        let (lead, trail) = accordion_insets(i, n, raised, accordion_padding);
        return Some(with_span(
            pr,
            axis,
            start + lead,
            (len - lead - trail).max(0.0),
        ));
    }
    let avail = (len - gaps.inner * (n - 1) as f64).max(0.0);
    let total: f64 = kids.iter().map(|&c| weight(tree, c)).sum();
    let before: f64 = kids[..i].iter().map(|&c| weight(tree, c)).sum();
    let a = boundary(avail, before, total, false);
    let b = boundary(avail, before + weight(tree, kids[i]), total, i + 1 == n);
    Some(with_span(
        pr,
        axis,
        start + a + i as f64 * gaps.inner,
        b - a,
    ))
}

/// Our fullscreen: the monitor's visible frame, optionally keeping the
/// outer gaps.
pub fn fullscreen_rect(monitor_visible: Rect, gaps: &Gaps, no_outer_gaps: bool) -> Rect {
    if no_outer_gaps {
        monitor_visible
    } else {
        gaps.outer.apply(monitor_visible)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::tests::wids;
    use tatami_protocol::Layout;

    fn run(
        t: &Tree,
        root: NodeId,
        area: Rect,
        gaps: &Gaps,
        focused: Option<NodeId>,
        pad: f64,
    ) -> Vec<Placement> {
        let mut out = Vec::new();
        compute(t, root, area, gaps, focused, pad, &mut out);
        out
    }

    fn gaps(inner: f64, outer: f64) -> Gaps {
        Gaps {
            inner,
            outer: Insets::uniform(outer),
        }
    }

    #[test]
    fn two_h_tiles_with_gaps_sum_exactly() {
        let ws = wids(2);
        let mut t = Tree::new();
        let root = t.new_root(Layout::HTiles);
        let a = t.insert_window(root, None, ws[0]);
        t.insert_window(root, Some(a), ws[1]);
        let out = run(
            &t,
            root,
            Rect::new(0.0, 25.0, 1512.0, 957.0),
            &gaps(10.0, 8.0),
            None,
            0.0,
        );
        assert_eq!(out.len(), 2);
        let (l, r) = (out[0].rect, out[1].rect);
        assert_eq!(l.x, 8.0);
        assert_eq!(l.y, 33.0);
        assert_eq!(l.h, 941.0);
        assert_eq!(r.x - l.right(), 10.0, "inner gap");
        assert_eq!(r.right(), 1504.0, "outer gap on the far side");
        assert_eq!(l.w + r.w + 10.0, 1512.0 - 16.0);
        assert!(out.iter().all(|p| p.z == 0));
    }

    #[test]
    fn weights_one_to_two() {
        let ws = wids(2);
        let mut t = Tree::new();
        let root = t.new_root(Layout::HTiles);
        let a = t.insert_window(root, None, ws[0]);
        let b = t.insert_window(root, Some(a), ws[1]);
        let sizes = |_: NodeId| Some(900.0);
        // 1:1 -> grow b by 300px of 900 => 1:2
        assert!(t.resize(b, Axis::H, 150.0, &sizes));
        let out = run(
            &t,
            root,
            Rect::new(0.0, 0.0, 900.0, 600.0),
            &Gaps::default(),
            None,
            0.0,
        );
        assert_eq!(out[0].rect.w, 300.0);
        assert_eq!(out[1].rect.w, 600.0);
        assert_eq!(out[1].rect.x, 300.0);
    }

    #[test]
    fn nested_v_in_h() {
        let ws = wids(3);
        let mut t = Tree::new();
        let root = t.new_root(Layout::HTiles);
        let a = t.insert_window(root, None, ws[0]);
        let b = t.insert_window(root, Some(a), ws[1]);
        t.split(b, tatami_protocol::action::SplitArg::Vertical);
        t.insert_window(root, Some(b), ws[2]);
        let out = run(
            &t,
            root,
            Rect::new(0.0, 0.0, 1000.0, 800.0),
            &gaps(10.0, 0.0),
            None,
            0.0,
        );
        let r = |w| out.iter().find(|p| p.window == w).unwrap().rect;
        assert_eq!(r(ws[0]), Rect::new(0.0, 0.0, 495.0, 800.0));
        assert_eq!(r(ws[1]), Rect::new(505.0, 0.0, 495.0, 395.0));
        assert_eq!(r(ws[2]), Rect::new(505.0, 405.0, 495.0, 395.0));
    }

    #[test]
    fn rounding_leaves_no_gap_and_no_overlap() {
        let ws = wids(7);
        let mut t = Tree::new();
        let root = t.new_root(Layout::VTiles);
        let mut last = None;
        for &w in &ws {
            last = Some(t.insert_window(root, last, w));
        }
        let out = run(
            &t,
            root,
            Rect::new(0.0, 0.0, 800.0, 1001.5),
            &gaps(7.0, 0.0),
            None,
            0.0,
        );
        for pair in out.windows(2) {
            assert_eq!(pair[1].rect.y - pair[0].rect.bottom(), 7.0);
        }
        for p in &out[..6] {
            assert_eq!(p.rect.h.fract(), 0.0, "interior sizes are whole points");
        }
        assert_eq!(
            out.last().unwrap().rect.bottom(),
            1001.5,
            "last child absorbs the fraction"
        );
    }

    #[test]
    fn accordion_shows_neighbour_slivers_and_raises_focused() {
        let ws = wids(3);
        let mut t = Tree::new();
        let root = t.new_root(Layout::HAccordion);
        let a = t.insert_window(root, None, ws[0]);
        let b = t.insert_window(root, Some(a), ws[1]);
        t.insert_window(root, Some(b), ws[2]);
        let area = Rect::new(0.0, 0.0, 1000.0, 600.0);
        let out = run(&t, root, area, &Gaps::default(), Some(b), 30.0);
        let p = |w| *out.iter().find(|p| p.window == w).unwrap();
        // Raised middle child: inset on both ends; neighbours show 30pt slivers.
        assert_eq!(p(ws[1]).rect, Rect::new(30.0, 0.0, 940.0, 600.0));
        assert_eq!(p(ws[0]).rect, Rect::new(0.0, 0.0, 970.0, 600.0));
        assert_eq!(p(ws[2]).rect, Rect::new(30.0, 0.0, 970.0, 600.0));
        assert_eq!((p(ws[1]).z, p(ws[0]).z, p(ws[2]).z), (0, 2, 2));
        assert!(
            out.iter().all(|p| p.rect.h == 600.0),
            "full area on the cross axis"
        );

        // Focus on the first: the next one shifts right so its sliver shows.
        let out = run(&t, root, area, &Gaps::default(), Some(a), 30.0);
        let p = |w| *out.iter().find(|p| p.window == w).unwrap();
        assert_eq!(p(ws[0]).rect, Rect::new(0.0, 0.0, 970.0, 600.0));
        assert_eq!(p(ws[1]).rect, Rect::new(60.0, 0.0, 940.0, 600.0));
        assert!(
            p(ws[1]).z < p(ws[2]).z,
            "nearest neighbour stacks above the far one"
        );
    }

    #[test]
    fn accordion_without_focus_raises_most_recent() {
        let ws = wids(2);
        let mut t = Tree::new();
        let root = t.new_root(Layout::VAccordion);
        let a = t.insert_window(root, None, ws[0]);
        let b = t.insert_window(root, Some(a), ws[1]);
        t.note_focus(b);
        let out = run(
            &t,
            root,
            Rect::new(0.0, 0.0, 800.0, 1000.0),
            &Gaps::default(),
            None,
            20.0,
        );
        let p = |w| *out.iter().find(|p| p.window == w).unwrap();
        assert_eq!(p(ws[1]).z, 0);
        assert_eq!(p(ws[1]).rect, Rect::new(0.0, 20.0, 800.0, 980.0));
    }

    #[test]
    fn nested_accordion_z_adds_up() {
        // h_tiles[a, v_accordion[b, c]] focused on a: c is behind b inside the accordion.
        let ws = wids(3);
        let mut t = Tree::new();
        let root = t.new_root(Layout::HTiles);
        let a = t.insert_window(root, None, ws[0]);
        let b = t.insert_window(root, Some(a), ws[1]);
        t.split(b, tatami_protocol::action::SplitArg::Vertical);
        t.insert_window(root, Some(b), ws[2]);
        t.set_layout(b, Layout::VAccordion);
        let out = run(
            &t,
            root,
            Rect::new(0.0, 0.0, 1000.0, 1000.0),
            &Gaps::default(),
            Some(a),
            10.0,
        );
        let z = |w| out.iter().find(|p| p.window == w).unwrap().z;
        assert_eq!((z(ws[0]), z(ws[1]), z(ws[2])), (0, 0, 2));
    }

    #[test]
    fn pending_leaves_reserve_space() {
        let ws = wids(1);
        let mut t = Tree::new();
        let root = t.new_root(Layout::HTiles);
        let a = t.insert_window(root, None, ws[0]);
        t.insert_pending(root, Some(a), 1);
        let out = run(
            &t,
            root,
            Rect::new(0.0, 0.0, 1000.0, 500.0),
            &Gaps::default(),
            None,
            0.0,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rect.w, 500.0);
    }

    #[test]
    fn node_rect_agrees_with_compute() {
        let ws = wids(4);
        let mut t = Tree::new();
        let root = t.new_root(Layout::HTiles);
        let a = t.insert_window(root, None, ws[0]);
        let b = t.insert_window(root, Some(a), ws[1]);
        t.split(b, tatami_protocol::action::SplitArg::Vertical);
        let c = t.insert_window(root, Some(b), ws[2]);
        t.insert_window(root, Some(c), ws[3]);
        t.resize(a, Axis::H, 77.0, &|_| Some(1000.0));
        let area = Rect::new(3.0, 30.0, 1001.0, 777.0);
        let g = gaps(9.0, 5.0);
        let out = run(&t, root, area, &g, None, 0.0);
        for p in &out {
            let leaf = t.leaf_of_window(root, p.window).unwrap();
            assert_eq!(node_rect(&t, root, area, &g, None, 0.0, leaf), Some(p.rect));
        }
        let v = t.parent(b).unwrap();
        let vr = node_rect(&t, root, area, &g, None, 0.0, v).unwrap();
        assert_eq!(vr.right(), area.right() - 5.0);
    }

    #[test]
    fn fullscreen_rect_with_and_without_outer_gaps() {
        let vis = Rect::new(0.0, 25.0, 1512.0, 957.0);
        let g = gaps(10.0, 8.0);
        assert_eq!(fullscreen_rect(vis, &g, true), vis);
        assert_eq!(
            fullscreen_rect(vis, &g, false),
            Rect::new(8.0, 33.0, 1496.0, 941.0)
        );
    }

    #[test]
    fn empty_or_dead_root_places_nothing() {
        let mut t = Tree::new();
        let root = t.new_root(Layout::HTiles);
        assert!(
            run(
                &t,
                root,
                Rect::new(0.0, 0.0, 10.0, 10.0),
                &Gaps::default(),
                None,
                0.0
            )
            .is_empty()
        );
        t.remove_root(root);
        assert!(
            run(
                &t,
                root,
                Rect::new(0.0, 0.0, 10.0, 10.0),
                &Gaps::default(),
                None,
                0.0
            )
            .is_empty()
        );
    }
}
