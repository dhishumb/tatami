//! The tiling tree: windows are leaves, containers lay their children out
//! along one axis with relative weights. One root per workspace; all roots
//! share one arena so moving a window between workspaces is a re-link, not
//! a copy.
//!
//! Commands (`move_leaf`, `join_with`, `set_layout`, `flatten`) leave the
//! tree normalized. `insert_*` and `remove_leaf` do not; the caller
//! normalizes once after a batch.

use slotmap::SlotMap;
use tatami_protocol::action::SplitArg;
use tatami_protocol::{Axis, Dir, Layout, Rect};

use crate::ids::{NodeId, WindowId};

/// Smallest share of its parent a resize may leave any child with.
const MIN_SHARE: f64 = 0.1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leaf {
    Window(WindowId),
    /// A slot reserved by a state restore for a window that has not
    /// appeared yet; it takes space so the layout does not jump when the
    /// window arrives.
    Pending(u32),
}

#[derive(Debug, Clone, PartialEq)]
pub enum NodeKind {
    Container {
        layout: Layout,
        children: Vec<NodeId>,
        /// Child last on the focus path. Picks the raised accordion child
        /// when focus is elsewhere and where `move` descends into a subtree.
        mru: Option<NodeId>,
    },
    Leaf(Leaf),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    /// Share of the parent's axis, relative to the siblings' weights.
    pub weight: f32,
    /// Created by `split` with a single child: normalize must not flatten
    /// it before the second child arrives. Cleared on the second attach.
    pub sticky: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Tree {
    nodes: SlotMap<NodeId, Node>,
}

fn tiles(axis: Axis) -> Layout {
    Layout::HTiles.with_axis(axis)
}

impl Tree {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn new_root(&mut self, layout: Layout) -> NodeId {
        self.alloc(NodeKind::Container {
            layout,
            children: Vec::new(),
            mru: None,
        })
    }

    fn alloc(&mut self, kind: NodeKind) -> NodeId {
        self.nodes.insert(Node {
            kind,
            parent: None,
            weight: 1.0,
            sticky: false,
        })
    }

    // ---- queries -------------------------------------------------------

    pub fn contains(&self, id: NodeId) -> bool {
        self.nodes.contains_key(id)
    }

    /// Live nodes across all roots (leak checks in tests).
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Panics on a dead id; use `contains` when the id may be stale.
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes.get(id).and_then(|n| n.parent)
    }

    /// Empty for leaves and dead ids.
    pub fn children(&self, id: NodeId) -> &[NodeId] {
        match self.nodes.get(id).map(|n| &n.kind) {
            Some(NodeKind::Container { children, .. }) => children,
            _ => &[],
        }
    }

    pub fn is_leaf(&self, id: NodeId) -> bool {
        matches!(self.nodes.get(id).map(|n| &n.kind), Some(NodeKind::Leaf(_)))
    }

    pub fn root_of(&self, mut id: NodeId) -> NodeId {
        while let Some(p) = self.parent(id) {
            id = p;
        }
        id
    }

    pub fn index_in_parent(&self, id: NodeId) -> Option<(NodeId, usize)> {
        let p = self.parent(id)?;
        let i = self.children(p).iter().position(|&c| c == id)?;
        Some((p, i))
    }

    /// Leaves in DFS order (allocates; the per-frame path never calls it).
    pub fn leaves(&self, root: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        self.for_each_leaf(root, &mut |l| out.push(l));
        out
    }

    pub fn for_each_leaf(&self, id: NodeId, f: &mut impl FnMut(NodeId)) {
        match self.nodes.get(id).map(|n| &n.kind) {
            Some(NodeKind::Leaf(_)) => f(id),
            Some(NodeKind::Container { children, .. }) => {
                for &c in children {
                    self.for_each_leaf(c, f);
                }
            }
            None => {}
        }
    }

    fn leaf_count(&self, id: NodeId) -> usize {
        let mut n = 0;
        self.for_each_leaf(id, &mut |_| n += 1);
        n
    }

    pub fn leaf_of_window(&self, root: NodeId, w: WindowId) -> Option<NodeId> {
        match &self.nodes.get(root)?.kind {
            NodeKind::Leaf(Leaf::Window(x)) if *x == w => Some(root),
            NodeKind::Leaf(_) => None,
            NodeKind::Container { children, .. } => {
                children.iter().find_map(|&c| self.leaf_of_window(c, w))
            }
        }
    }

    pub fn window_of(&self, leaf: NodeId) -> Option<WindowId> {
        match &self.nodes.get(leaf)?.kind {
            NodeKind::Leaf(Leaf::Window(w)) => Some(*w),
            _ => None,
        }
    }

    pub fn pending_of(&self, leaf: NodeId) -> Option<u32> {
        match &self.nodes.get(leaf)?.kind {
            NodeKind::Leaf(Leaf::Pending(s)) => Some(*s),
            _ => None,
        }
    }

    /// Leaves always have a parent (the root at least).
    pub fn container_of(&self, leaf: NodeId) -> NodeId {
        self.parent(leaf).expect("a leaf always has a parent")
    }

    fn container_layout(&self, id: NodeId) -> Option<Layout> {
        match &self.nodes.get(id)?.kind {
            NodeKind::Container { layout, .. } => Some(*layout),
            NodeKind::Leaf(_) => None,
        }
    }

    /// A container's layout; for a leaf, its parent's.
    pub fn layout_of(&self, node: NodeId) -> Layout {
        self.container_layout(node)
            .or_else(|| self.parent(node).and_then(|p| self.container_layout(p)))
            .unwrap_or(Layout::HTiles)
    }

    pub fn mru_of(&self, container: NodeId) -> Option<NodeId> {
        match &self.nodes.get(container)?.kind {
            NodeKind::Container { mru, children, .. } => mru.filter(|m| children.contains(m)),
            NodeKind::Leaf(_) => None,
        }
    }

    // ---- low-level linking -------------------------------------------------

    fn kids_mut(&mut self, id: NodeId) -> &mut Vec<NodeId> {
        match &mut self.nodes[id].kind {
            NodeKind::Container { children, .. } => children,
            NodeKind::Leaf(_) => unreachable!("leaves have no children"),
        }
    }

    fn set_mru_if(&mut self, container: NodeId, was: NodeId, now: Option<NodeId>) {
        if let Some(NodeKind::Container { mru, .. }) =
            self.nodes.get_mut(container).map(|n| &mut n.kind)
            && *mru == Some(was)
        {
            *mru = now;
        }
    }

    fn attach(&mut self, parent: NodeId, index: usize, child: NodeId, weight: f32) {
        let n = &mut self.nodes[child];
        n.parent = Some(parent);
        n.weight = weight;
        let kids = self.kids_mut(parent);
        let at = index.min(kids.len());
        kids.insert(at, child);
        if kids.len() > 1 {
            self.nodes[parent].sticky = false;
        }
    }

    fn detach(&mut self, child: NodeId) -> Option<(NodeId, usize)> {
        let (p, i) = self.index_in_parent(child)?;
        self.kids_mut(p).remove(i);
        self.set_mru_if(p, child, None);
        self.nodes[child].parent = None;
        Some((p, i))
    }

    /// Put `new` into `old`'s slot with `weight`; `old` is left unlinked.
    fn replace_slot(&mut self, old: NodeId, new: NodeId, weight: f32) {
        let (p, i) = self
            .index_in_parent(old)
            .expect("replace_slot on an unlinked node");
        self.kids_mut(p)[i] = new;
        self.set_mru_if(p, old, Some(new));
        self.nodes[old].parent = None;
        let n = &mut self.nodes[new];
        n.parent = Some(p);
        n.weight = weight;
    }

    /// Weight a newcomer gets so it takes an average share.
    fn avg_weight(&self, parent: NodeId) -> f32 {
        let kids = self.children(parent);
        if kids.is_empty() {
            1.0
        } else {
            kids.iter().map(|&k| self.nodes[k].weight).sum::<f32>() / kids.len() as f32
        }
    }

    // ---- insertion / removal -------------------------------------------

    fn insert_leaf(&mut self, root: NodeId, after: Option<NodeId>, leaf: Leaf) -> NodeId {
        let anchor = after
            .filter(|&a| self.is_leaf(a) && self.root_of(a) == root)
            .and_then(|a| self.index_in_parent(a));
        let (parent, index) = match anchor {
            Some((p, i)) => (p, i + 1),
            None => (root, usize::MAX),
        };
        let weight = self.avg_weight(parent);
        let id = self.alloc(NodeKind::Leaf(leaf));
        self.attach(parent, index, id, weight);
        id
    }

    /// Sibling right after the focused leaf (never wraps it), or appended to
    /// the root when there is no usable focus.
    pub fn insert_window(&mut self, root: NodeId, after: Option<NodeId>, w: WindowId) -> NodeId {
        self.insert_leaf(root, after, Leaf::Window(w))
    }

    pub fn insert_pending(&mut self, root: NodeId, after: Option<NodeId>, spec: u32) -> NodeId {
        self.insert_leaf(root, after, Leaf::Pending(spec))
    }

    pub fn replace_pending(&mut self, leaf: NodeId, w: WindowId) {
        if let Some(n) = self.nodes.get_mut(leaf)
            && matches!(n.kind, NodeKind::Leaf(Leaf::Pending(_)))
        {
            n.kind = NodeKind::Leaf(Leaf::Window(w));
        }
    }

    /// Append a container under `parent` with `weight` (rebuilding a saved
    /// tree). Not normalized; the caller normalizes once at the end.
    pub fn push_container(&mut self, parent: NodeId, layout: Layout, weight: f32) -> NodeId {
        let id = self.alloc(NodeKind::Container {
            layout,
            children: Vec::new(),
            mru: None,
        });
        self.attach(parent, usize::MAX, id, weight);
        id
    }

    /// Append a leaf under `parent` with `weight` (rebuilding a saved tree).
    pub fn push_leaf(&mut self, parent: NodeId, leaf: Leaf, weight: f32) -> NodeId {
        let id = self.alloc(NodeKind::Leaf(leaf));
        self.attach(parent, usize::MAX, id, weight);
        id
    }

    pub fn set_weight(&mut self, node: NodeId, weight: f32) {
        if let Some(n) = self.nodes.get_mut(node) {
            n.weight = weight;
        }
    }

    /// Unlink and free a leaf. Call `normalize` on its root afterwards.
    pub fn remove_leaf(&mut self, leaf: NodeId) {
        if self.is_leaf(leaf) {
            self.detach(leaf);
            self.nodes.remove(leaf);
        }
    }

    /// Free a whole tree (a garbage-collected workspace).
    pub fn remove_root(&mut self, root: NodeId) {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if let Some(n) = self.nodes.remove(id)
                && let NodeKind::Container { children, .. } = n.kind
            {
                stack.extend(children);
            }
        }
    }

    /// Append every child of `from` to `into` (merging two workspaces), then
    /// normalize `into`. `from` is left as an empty root.
    pub fn move_children(&mut self, from: NodeId, into: NodeId) {
        let kids = std::mem::take(self.kids_mut(from));
        if let NodeKind::Container { mru, .. } = &mut self.nodes[from].kind {
            *mru = None;
        }
        for k in kids {
            let w = self.nodes[k].weight;
            self.attach(into, usize::MAX, k, w);
        }
        self.normalize(into);
    }

    /// Record `leaf` as the most recent child at every level above it.
    pub fn note_focus(&mut self, leaf: NodeId) {
        let mut cur = leaf;
        while let Some(p) = self.parent(cur) {
            if let NodeKind::Container { mru, .. } = &mut self.nodes[p].kind {
                *mru = Some(cur);
            }
            cur = p;
        }
    }

    // ---- normalization -------------------------------------------------

    /// Drop empty containers (never the root), flatten single-child
    /// containers into their slot, and merge a container into its parent
    /// when both have the same layout. Sticky (fresh `split`) containers are
    /// exempt from flattening and merging. A root left with a single
    /// container child adopts that child's layout and children so the
    /// geometry is unchanged and the root id stays stable.
    pub fn normalize(&mut self, root: NodeId) {
        if !self.contains(root) {
            return;
        }
        self.normalize_children(root);
        while let &[only] = self.children(root) {
            if self.is_leaf(only) || self.nodes[only].sticky {
                break;
            }
            let (layout, kids, mru) = match &mut self.nodes[only].kind {
                NodeKind::Container {
                    layout,
                    children,
                    mru,
                } => (*layout, std::mem::take(children), *mru),
                NodeKind::Leaf(_) => unreachable!(),
            };
            for &k in &kids {
                self.nodes[k].parent = Some(root);
            }
            self.nodes[root].kind = NodeKind::Container {
                layout,
                children: kids,
                mru,
            };
            self.nodes.remove(only);
        }
    }

    fn normalize_children(&mut self, id: NodeId) {
        let layout = self.container_layout(id);
        let mut i = 0;
        while let Some(&c) = self.children(id).get(i) {
            let Some(c_layout) = self.container_layout(c) else {
                i += 1;
                continue;
            };
            self.normalize_children(c);
            let sticky = self.nodes[c].sticky;
            // Each arm except the last rewrites slot `i`, so it is examined again.
            match *self.children(c) {
                [] => {
                    self.detach(c);
                    self.nodes.remove(c);
                }
                [only] if !sticky => {
                    let w = self.nodes[c].weight;
                    self.replace_slot(c, only, w);
                    self.nodes.remove(c);
                }
                _ if !sticky && Some(c_layout) == layout => self.splice_into_parent(c),
                _ => i += 1,
            }
        }
    }

    /// Replace `c` by its children, scaling their weights so their combined
    /// share of the parent equals `c`'s.
    fn splice_into_parent(&mut self, c: NodeId) {
        let (p, i) = self.index_in_parent(c).expect("splice of an unlinked node");
        let wc = f64::from(self.nodes[c].weight);
        let c_mru = self.mru_of(c);
        let kids = std::mem::take(self.kids_mut(c));
        let total: f64 = kids.iter().map(|&k| f64::from(self.nodes[k].weight)).sum();
        for &k in &kids {
            let n = &mut self.nodes[k];
            n.parent = Some(p);
            n.weight = if total > 0.0 {
                (f64::from(n.weight) * wc / total) as f32
            } else {
                wc as f32
            };
        }
        self.kids_mut(p).splice(i..=i, kids);
        self.set_mru_if(p, c, c_mru);
        self.nodes.remove(c);
    }

    // ---- navigation ----------------------------------------------------

    /// Nearest ancestor-or-self of `leaf` that has a sibling in `dir` within
    /// a parent laid out along `dir`'s axis: `(parent, sibling)`.
    fn sibling_in_dir(&self, leaf: NodeId, dir: Dir) -> Option<(NodeId, NodeId)> {
        let mut cur = leaf;
        loop {
            let (p, i) = self.index_in_parent(cur)?;
            if self.layout_of(p).axis() == dir.axis() {
                let kids = self.children(p);
                let j = i
                    .checked_add_signed(dir.sign() as isize)
                    .filter(|&j| j < kids.len());
                if let Some(j) = j {
                    return Some((p, kids[j]));
                }
            }
            cur = p;
        }
    }

    /// First (or last) leaf of a subtree.
    fn edge_leaf(&self, mut id: NodeId, first: bool) -> Option<NodeId> {
        loop {
            if self.is_leaf(id) {
                return Some(id);
            }
            let kids = self.children(id);
            id = *(if first { kids.first() } else { kids.last() })?;
        }
    }

    /// The window to focus in `dir`. The tree picks the adjacent subtree
    /// (so overlapping accordion children still navigate in order); frames
    /// pick the leaf inside it: one overlapping ours on the orthogonal axis,
    /// nearest first, longest overlap breaking ties.
    pub fn neighbor(
        &self,
        leaf: NodeId,
        dir: Dir,
        frames: &impl Fn(WindowId) -> Option<Rect>,
    ) -> Option<NodeId> {
        let (_, target) = self.sibling_in_dir(leaf, dir)?;
        let fallback = self.edge_leaf(target, dir.sign() > 0);
        let Some(here) = self.window_of(leaf).and_then(frames) else {
            return fallback;
        };
        // (leaf, overlaps, distance, overlap)
        let mut best: Option<(NodeId, bool, f64, f64)> = None;
        self.for_each_leaf(target, &mut |l| {
            let Some(r) = self.window_of(l).and_then(frames) else {
                return;
            };
            let (dist, overlap) = match dir {
                Dir::Right => (
                    r.x - here.right(),
                    r.bottom().min(here.bottom()) - r.y.max(here.y),
                ),
                Dir::Left => (
                    here.x - r.right(),
                    r.bottom().min(here.bottom()) - r.y.max(here.y),
                ),
                Dir::Down => (
                    r.y - here.bottom(),
                    r.right().min(here.right()) - r.x.max(here.x),
                ),
                Dir::Up => (
                    here.y - r.bottom(),
                    r.right().min(here.right()) - r.x.max(here.x),
                ),
            };
            let ov = overlap > 0.0;
            let better = match best {
                None => true,
                Some((_, bov, bd, bo)) => {
                    (ov && !bov)
                        || (ov == bov
                            && (dist < bd - 0.5 || ((dist - bd).abs() <= 0.5 && overlap > bo)))
                }
            };
            if better {
                best = Some((l, ov, dist, overlap));
            }
        });
        best.map(|b| b.0).or(fallback)
    }

    pub fn dfs_next(&self, leaf: NodeId, forward: bool, wrap: bool) -> Option<NodeId> {
        let all = self.leaves(self.root_of(leaf));
        let i = all.iter().position(|&l| l == leaf)?;
        let n = all.len();
        let j = match (forward, wrap) {
            (true, _) if i + 1 < n => i + 1,
            (true, true) => 0,
            (false, _) if i > 0 => i - 1,
            (false, true) => n - 1,
            _ => return None,
        };
        (all[j] != leaf).then_some(all[j])
    }

    // ---- structural commands -------------------------------------------

    /// AeroSpace/i3 `move`: swap with a sibling leaf in `dir`; enter a
    /// sibling container at its near end; at the parent's edge hop out next
    /// to the nearest ancestor laid out along `dir`; at the root edge wrap
    /// the root in a new container along `dir`. Normalizes.
    pub fn move_leaf(&mut self, leaf: NodeId, dir: Dir) -> bool {
        let Some((parent, idx)) = self.index_in_parent(leaf) else {
            return false;
        };
        let root = self.root_of(leaf);
        let axis = dir.axis();
        let positive = dir.sign() > 0;

        if self.layout_of(parent).axis() == axis {
            let kids = self.children(parent);
            if let Some(j) = idx
                .checked_add_signed(dir.sign() as isize)
                .filter(|&j| j < kids.len())
            {
                let sib = kids[j];
                if self.is_leaf(sib) {
                    self.kids_mut(parent).swap(idx, j);
                } else {
                    let (dest, at) = self.deep_move_in_target(sib, axis, positive);
                    self.detach(leaf);
                    let w = self.avg_weight(dest);
                    self.attach(dest, at, leaf, w);
                    self.normalize(root);
                }
                return true;
            }
        }

        // Hop out: the first ancestor X (from the parent up) whose own
        // parent runs along `axis` takes the leaf right before/after X.
        let mut x = parent;
        while let Some((gp, xi)) = self.index_in_parent(x) {
            if self.layout_of(gp).axis() == axis {
                self.detach(leaf);
                let w = self.avg_weight(gp);
                self.attach(gp, xi + usize::from(positive), leaf, w);
                self.normalize(root);
                return true;
            }
            x = gp;
        }

        // Root edge: wrap the root's content (keeping the root id stable).
        if self.layout_of(root).axis() == axis || self.leaf_count(root) < 2 {
            return false;
        }
        self.detach(leaf);
        let (old_layout, kids, mru) = match &mut self.nodes[root].kind {
            NodeKind::Container {
                layout,
                children,
                mru,
            } => (*layout, std::mem::take(children), mru.take()),
            NodeKind::Leaf(_) => unreachable!("roots are containers"),
        };
        let wrapped = self.alloc(NodeKind::Container {
            layout: old_layout,
            children: kids,
            mru,
        });
        for k in self.children(wrapped).to_vec() {
            self.nodes[k].parent = Some(wrapped);
        }
        if let NodeKind::Container { layout, .. } = &mut self.nodes[root].kind {
            *layout = tiles(axis);
        }
        self.attach(root, 0, wrapped, 1.0);
        self.attach(root, if positive { 1 } else { 0 }, leaf, 1.0);
        self.normalize(root);
        true
    }

    /// Where a leaf moving in `dir` lands inside sibling subtree `sib`:
    /// the near end of the first container along `axis`, else right after
    /// the most recent leaf.
    fn deep_move_in_target(&self, sib: NodeId, axis: Axis, positive: bool) -> (NodeId, usize) {
        let mut t = sib;
        loop {
            match &self.nodes[t].kind {
                NodeKind::Leaf(_) => {
                    let (p, i) = self.index_in_parent(t).expect("leaf has a parent");
                    return (p, i + 1);
                }
                NodeKind::Container {
                    layout,
                    children,
                    mru,
                } => {
                    if layout.axis() == axis || children.is_empty() {
                        return (t, if positive { 0 } else { children.len() });
                    }
                    t = mru.filter(|m| children.contains(m)).unwrap_or(children[0]);
                }
            }
        }
    }

    /// Exchange two nodes' positions. Weights stay with the slots so the
    /// layout geometry is unchanged; only the occupants trade places.
    pub fn swap_leaves(&mut self, a: NodeId, b: NodeId) {
        if a == b {
            return;
        }
        let (Some((pa, ia)), Some((pb, ib))) = (self.index_in_parent(a), self.index_in_parent(b))
        else {
            return;
        };
        self.kids_mut(pa)[ia] = b;
        self.kids_mut(pb)[ib] = a;
        let (wa, wb) = (self.nodes[a].weight, self.nodes[b].weight);
        let na = &mut self.nodes[a];
        na.parent = Some(pb);
        na.weight = wb;
        let nb = &mut self.nodes[b];
        nb.parent = Some(pa);
        nb.weight = wa;
    }

    /// AeroSpace `join-with`: the nearest ancestor-or-self with a sibling in
    /// `dir` is paired with that sibling, and the leaf joins them in a new
    /// container of the opposite orientation. Normalizes.
    pub fn join_with(&mut self, leaf: NodeId, dir: Dir) -> bool {
        let Some((parent, target)) = self.sibling_in_dir(leaf, dir) else {
            return false;
        };
        let root = self.root_of(leaf);
        let p_axis = self.layout_of(parent).axis();
        self.detach(leaf);
        let tw = self.nodes[target].weight;
        let (_, ti) = self.detach(target).expect("sibling is linked");
        let joined = self.alloc(NodeKind::Container {
            layout: tiles(p_axis.other()),
            children: Vec::new(),
            mru: None,
        });
        self.attach(parent, ti, joined, tw);
        self.attach(joined, 0, target, 1.0);
        let at = if dir.sign() > 0 { 0 } else { 1 };
        self.attach(joined, at, leaf, 1.0);
        self.normalize(root);
        true
    }

    /// Wrap `leaf` in a fresh (sticky) container along the requested axis
    /// so the next window inserted after it nests. A single-child parent is
    /// just reoriented. Splitting along the parent's own axis is a no-op:
    /// normalization would merge that nesting straight back.
    pub fn split(&mut self, leaf: NodeId, arg: SplitArg) -> bool {
        let Some((parent, _)) = self.index_in_parent(leaf) else {
            return false;
        };
        let p_layout = self.layout_of(parent);
        let axis = match arg {
            SplitArg::Horizontal => Axis::H,
            SplitArg::Vertical => Axis::V,
            SplitArg::Opposite => p_layout.axis().other(),
        };
        if axis == p_layout.axis() {
            return false;
        }
        if self.children(parent).len() == 1 {
            if let NodeKind::Container { layout, .. } = &mut self.nodes[parent].kind {
                *layout = p_layout.with_axis(axis);
            }
            return true;
        }
        let w = self.nodes[leaf].weight;
        let c = self.alloc(NodeKind::Container {
            layout: tiles(axis),
            children: Vec::new(),
            mru: Some(leaf),
        });
        self.replace_slot(leaf, c, w);
        self.attach(c, 0, leaf, 1.0);
        self.nodes[c].sticky = true;
        true
    }

    /// Grow (or shrink, negative `delta_px`) the nearest ancestor-or-self of
    /// `leaf` whose parent tiles along `axis` and has siblings. Siblings
    /// give up the space evenly; nobody drops below `MIN_SHARE` of the
    /// parent. `sizes` reports a node's current length along `axis`.
    pub fn resize(
        &mut self,
        leaf: NodeId,
        axis: Axis,
        delta_px: f64,
        sizes: &impl Fn(NodeId) -> Option<f64>,
    ) -> bool {
        let mut a = leaf;
        let p = loop {
            let Some(p) = self.parent(a) else {
                return false;
            };
            let l = self.layout_of(p);
            if l.axis() == axis && !l.is_accordion() && self.children(p).len() > 1 {
                break p;
            }
            a = p;
        };
        let Some(total_px) = sizes(p).filter(|&t| t > 0.0) else {
            return false;
        };
        let kids = self.children(p);
        let n = kids.len();
        let sum: f64 = kids.iter().map(|&k| f64::from(self.nodes[k].weight)).sum();
        let min_w = MIN_SHARE * sum;
        let wa = f64::from(self.nodes[a].weight);
        let min_sib = kids
            .iter()
            .filter(|&&k| k != a)
            .map(|&k| f64::from(self.nodes[k].weight))
            .fold(f64::INFINITY, f64::min);
        let hi = ((min_sib - min_w) * (n - 1) as f64).max(0.0);
        let lo = -(wa - min_w).max(0.0);
        let d = (delta_px / total_px * sum).clamp(lo, hi);
        if d.abs() < 1e-6 {
            return false;
        }
        let share = d / (n - 1) as f64;
        for k in self.children(p).to_vec() {
            let n = &mut self.nodes[k];
            let w = f64::from(n.weight) + if k == a { d } else { -share };
            n.weight = w as f32;
        }
        true
    }

    /// Set a container's layout; a leaf targets its parent. Normalizes, so a
    /// container switched to its parent's layout merges into it.
    pub fn set_layout(&mut self, node: NodeId, layout: Layout) {
        let target = if self.is_leaf(node) {
            self.parent(node)
        } else {
            Some(node)
        };
        let Some(target) = target else { return };
        if let Some(NodeKind::Container { layout: l, .. }) =
            self.nodes.get_mut(target).map(|n| &mut n.kind)
        {
            *l = layout;
            let root = self.root_of(target);
            self.normalize(root);
        }
    }

    /// Every leaf becomes a direct child of the root with equal weight.
    pub fn flatten(&mut self, root: NodeId) {
        if self.container_layout(root).is_none() {
            return;
        }
        let leaves = self.leaves(root);
        let mut dead = Vec::new();
        for &c in self.children(root) {
            if !self.is_leaf(c) {
                dead.push(c);
            }
        }
        while let Some(c) = dead.pop() {
            if let Some(n) = self.nodes.remove(c)
                && let NodeKind::Container { children, .. } = n.kind
            {
                dead.extend(children.into_iter().filter(|&k| !self.is_leaf(k)));
            }
        }
        for &l in &leaves {
            let n = &mut self.nodes[l];
            n.parent = Some(root);
            n.weight = 1.0;
        }
        if let NodeKind::Container { children, mru, .. } = &mut self.nodes[root].kind {
            *children = leaves;
            *mru = None;
        }
        self.nodes[root].sticky = false;
    }

    /// All weights back to 1.0.
    pub fn balance(&mut self, root: NodeId) {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let Some(n) = self.nodes.get_mut(id) else {
                continue;
            };
            n.weight = 1.0;
            if let NodeKind::Container { children, .. } = &n.kind {
                stack.extend_from_slice(children);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use slotmap::SlotMap;
    use tatami_protocol::Layout::*;

    pub(crate) fn wids(n: usize) -> Vec<WindowId> {
        let mut m: SlotMap<WindowId, ()> = SlotMap::with_key();
        (0..n).map(|_| m.insert(())).collect()
    }

    /// `h_tiles[a, v_tiles[b, c]]`; windows are named by their index in `ws`.
    pub(crate) fn show(t: &Tree, id: NodeId, ws: &[WindowId]) -> String {
        match &t.node(id).kind {
            NodeKind::Leaf(Leaf::Window(w)) => {
                let i = ws.iter().position(|x| x == w).unwrap();
                ((b'a' + i as u8) as char).to_string()
            }
            NodeKind::Leaf(Leaf::Pending(s)) => format!("?{s}"),
            NodeKind::Container {
                layout, children, ..
            } => {
                let inner: Vec<String> = children.iter().map(|&c| show(t, c, ws)).collect();
                format!("{layout}[{}]", inner.join(", "))
            }
        }
    }

    /// Hand-build a container under `parent` (bypassing normalization).
    fn container(t: &mut Tree, parent: NodeId, layout: Layout) -> NodeId {
        let c = t.alloc(NodeKind::Container {
            layout,
            children: Vec::new(),
            mru: None,
        });
        t.attach(parent, usize::MAX, c, 1.0);
        c
    }

    fn leaf(t: &mut Tree, parent: NodeId, w: WindowId) -> NodeId {
        let l = t.alloc(NodeKind::Leaf(Leaf::Window(w)));
        t.attach(parent, usize::MAX, l, 1.0);
        l
    }

    fn row(n: usize) -> (Tree, NodeId, Vec<NodeId>, Vec<WindowId>) {
        let ws = wids(n);
        let mut t = Tree::new();
        let root = t.new_root(HTiles);
        let mut last = None;
        let leaves = ws
            .iter()
            .map(|&w| {
                let l = t.insert_window(root, last, w);
                last = Some(l);
                l
            })
            .collect();
        (t, root, leaves, ws)
    }

    #[test]
    fn insert_into_empty_root_then_after_focused() {
        let ws = wids(3);
        let mut t = Tree::new();
        let root = t.new_root(HTiles);
        let a = t.insert_window(root, None, ws[0]);
        assert_eq!(t.children(root), &[a]);
        t.insert_window(root, Some(a), ws[1]);
        t.insert_window(root, Some(a), ws[2]);
        assert_eq!(show(&t, root, &ws), "h_tiles[a, c, b]");
        assert_eq!(t.container_of(a), root);
    }

    #[test]
    fn insert_without_focus_appends_and_inherits_average_weight() {
        let (mut t, root, l, ws) = row(2);
        t.nodes[l[0]].weight = 2.0;
        t.nodes[l[1]].weight = 1.0;
        let extra = wids(3)[2];
        let c = t.insert_window(root, None, extra);
        assert_eq!(*t.children(root).last().unwrap(), c);
        assert!((t.node(c).weight - 1.5).abs() < 1e-6);
        assert_eq!(t.children(root).len(), ws.len() + 1);
    }

    #[test]
    fn insert_after_leaf_of_other_root_falls_back_to_append() {
        let (mut t, root, l, _) = row(2);
        let other = t.new_root(VTiles);
        let w = wids(1)[0];
        let x = t.insert_window(other, Some(l[0]), w);
        assert_eq!(t.parent(x), Some(other));
        assert_eq!(t.children(root).len(), 2);
    }

    #[test]
    fn remove_then_normalize_flattens_single_child() {
        let ws = wids(3);
        let mut t = Tree::new();
        let root = t.new_root(HTiles);
        leaf(&mut t, root, ws[0]);
        let v = container(&mut t, root, VTiles);
        t.nodes[v].weight = 3.0;
        leaf(&mut t, v, ws[1]);
        let c = leaf(&mut t, v, ws[2]);
        t.remove_leaf(c);
        t.normalize(root);
        assert_eq!(show(&t, root, &ws), "h_tiles[a, b]");
        let b = t.leaf_of_window(root, ws[1]).unwrap();
        assert_eq!(
            t.node(b).weight,
            3.0,
            "survivor inherits the container's share"
        );
        assert_eq!(t.node_count(), 3);
    }

    #[test]
    fn normalize_drops_empty_containers_but_never_the_root() {
        let ws = wids(1);
        let mut t = Tree::new();
        let root = t.new_root(HTiles);
        leaf(&mut t, root, ws[0]);
        let v = container(&mut t, root, VTiles);
        container(&mut t, v, HTiles);
        t.normalize(root);
        assert_eq!(show(&t, root, &ws), "h_tiles[a]");
        let a = t.leaf_of_window(root, ws[0]).unwrap();
        t.remove_leaf(a);
        t.normalize(root);
        assert!(t.contains(root));
        assert!(t.children(root).is_empty());
        assert_eq!(t.node_count(), 1);
    }

    #[test]
    fn normalize_merges_same_layout_nesting_preserving_shares() {
        let ws = wids(3);
        let mut t = Tree::new();
        let root = t.new_root(HTiles);
        leaf(&mut t, root, ws[0]);
        let inner = container(&mut t, root, HTiles);
        leaf(&mut t, inner, ws[1]);
        leaf(&mut t, inner, ws[2]);
        t.normalize(root);
        assert_eq!(show(&t, root, &ws), "h_tiles[a, b, c]");
        let w: Vec<f32> = t.children(root).iter().map(|&c| t.node(c).weight).collect();
        assert_eq!(w, vec![1.0, 0.5, 0.5]);
    }

    #[test]
    fn normalize_does_not_merge_accordion_into_tiles_of_same_axis() {
        let ws = wids(3);
        let mut t = Tree::new();
        let root = t.new_root(HTiles);
        leaf(&mut t, root, ws[0]);
        let acc = container(&mut t, root, HAccordion);
        leaf(&mut t, acc, ws[1]);
        leaf(&mut t, acc, ws[2]);
        t.normalize(root);
        assert_eq!(show(&t, root, &ws), "h_tiles[a, h_accordion[b, c]]");
    }

    #[test]
    fn root_adopts_its_only_container_child_keeping_geometry_and_id() {
        let ws = wids(2);
        let mut t = Tree::new();
        let root = t.new_root(HTiles);
        let v = container(&mut t, root, VTiles);
        leaf(&mut t, v, ws[0]);
        leaf(&mut t, v, ws[1]);
        t.normalize(root);
        assert_eq!(show(&t, root, &ws), "v_tiles[a, b]");
        assert!(t.children(root).iter().all(|&c| t.parent(c) == Some(root)));
        assert_eq!(t.node_count(), 3);
    }

    #[test]
    fn split_then_insert_nests_and_normalize_keeps_fresh_split() {
        let (mut t, root, l, _) = row(2);
        let ws = wids(3);
        assert!(t.split(l[0], SplitArg::Vertical));
        t.normalize(root);
        assert_eq!(
            show(&t, root, &ws),
            "h_tiles[v_tiles[a], b]",
            "sticky container survives normalize"
        );
        let c = t.insert_window(root, Some(l[0]), ws[2]);
        t.normalize(root);
        assert_eq!(show(&t, root, &ws), "h_tiles[v_tiles[a, c], b]");
        assert!(!t.node(t.container_of(l[0])).sticky);
        // Closing the new window flattens the split away again.
        t.remove_leaf(c);
        t.normalize(root);
        assert_eq!(show(&t, root, &ws), "h_tiles[a, b]");
    }

    #[test]
    fn split_opposite_same_axis_and_single_child() {
        let (mut t, root, l, ws) = row(2);
        assert!(
            !t.split(l[0], SplitArg::Horizontal),
            "same axis as parent is a no-op"
        );
        assert!(t.split(l[0], SplitArg::Opposite));
        assert_eq!(show(&t, root, &ws), "h_tiles[v_tiles[a], b]");

        let (mut t, root, l, ws) = row(1);
        assert!(
            t.split(l[0], SplitArg::Vertical),
            "single child: parent is reoriented"
        );
        assert_eq!(show(&t, root, &ws), "v_tiles[a]");
    }

    #[test]
    fn move_swaps_with_sibling_leaf() {
        let (mut t, root, l, ws) = row(3);
        assert!(t.move_leaf(l[0], Dir::Right));
        assert_eq!(show(&t, root, &ws), "h_tiles[b, a, c]");
        assert!(t.move_leaf(l[0], Dir::Left));
        assert_eq!(show(&t, root, &ws), "h_tiles[a, b, c]");
        assert!(
            !t.move_leaf(l[0], Dir::Left),
            "root edge along the root's axis"
        );
    }

    #[test]
    fn move_hops_out_to_the_grandparent() {
        let ws = wids(3);
        let mut t = Tree::new();
        let root = t.new_root(HTiles);
        leaf(&mut t, root, ws[0]);
        let v = container(&mut t, root, VTiles);
        let b = leaf(&mut t, v, ws[1]);
        leaf(&mut t, v, ws[2]);
        assert!(t.move_leaf(b, Dir::Right));
        assert_eq!(show(&t, root, &ws), "h_tiles[a, c, b]");

        let mut t = Tree::new();
        let root = t.new_root(HTiles);
        leaf(&mut t, root, ws[0]);
        let v = container(&mut t, root, VTiles);
        let b = leaf(&mut t, v, ws[1]);
        leaf(&mut t, v, ws[2]);
        assert!(t.move_leaf(b, Dir::Left));
        assert_eq!(show(&t, root, &ws), "h_tiles[a, b, c]");
    }

    #[test]
    fn move_up_at_v_edge_hops_out_of_nested_v() {
        // v[ h[a, v[b, c]], d ]: moving b up leaves the inner v and lands above the h.
        let ws = wids(4);
        let mut t = Tree::new();
        let root = t.new_root(VTiles);
        let h = container(&mut t, root, HTiles);
        leaf(&mut t, h, ws[0]);
        let v = container(&mut t, h, VTiles);
        let b = leaf(&mut t, v, ws[1]);
        leaf(&mut t, v, ws[2]);
        leaf(&mut t, root, ws[3]);
        assert!(t.move_leaf(b, Dir::Up));
        assert_eq!(show(&t, root, &ws), "v_tiles[b, h_tiles[a, c], d]");
    }

    #[test]
    fn move_wraps_the_root_when_orientation_differs() {
        let (mut t, root, l, ws) = row(3);
        assert!(t.move_leaf(l[0], Dir::Down));
        assert_eq!(show(&t, root, &ws), "v_tiles[h_tiles[b, c], a]");
        // Moving up enters the h container next to its most recent child (i3/AeroSpace).
        assert!(t.move_leaf(l[0], Dir::Up));
        assert_eq!(show(&t, root, &ws), "h_tiles[b, a, c]");

        let (mut t, _, l, _) = row(1);
        assert!(!t.move_leaf(l[0], Dir::Down), "alone: nothing to wrap");
    }

    #[test]
    fn move_into_sibling_container_enters_at_near_end() {
        // h[a, v[b, c]]: a moving right enters v after its most recent child.
        let ws = wids(4);
        let mut t = Tree::new();
        let root = t.new_root(HTiles);
        let a = leaf(&mut t, root, ws[0]);
        let v = container(&mut t, root, VTiles);
        leaf(&mut t, v, ws[1]);
        let c = leaf(&mut t, v, ws[2]);
        leaf(&mut t, root, ws[3]);
        t.note_focus(c);
        assert!(t.move_leaf(a, Dir::Right));
        assert_eq!(show(&t, root, &ws), "h_tiles[v_tiles[b, c, a], d]");

        // h[a, h_accordion[b, c]]: same axis, so it lands first in the accordion.
        let mut t = Tree::new();
        let root = t.new_root(HTiles);
        let a = leaf(&mut t, root, ws[0]);
        let acc = container(&mut t, root, HAccordion);
        leaf(&mut t, acc, ws[1]);
        leaf(&mut t, acc, ws[2]);
        assert!(t.move_leaf(a, Dir::Right));
        assert_eq!(show(&t, root, &ws), "h_accordion[a, b, c]");
    }

    fn grid() -> (Tree, NodeId, Vec<NodeId>, Vec<WindowId>) {
        // h[ v[a, b], v[c, d] ]
        let ws = wids(4);
        let mut t = Tree::new();
        let root = t.new_root(HTiles);
        let v1 = container(&mut t, root, VTiles);
        let a = leaf(&mut t, v1, ws[0]);
        let b = leaf(&mut t, v1, ws[1]);
        let v2 = container(&mut t, root, VTiles);
        let c = leaf(&mut t, v2, ws[2]);
        let d = leaf(&mut t, v2, ws[3]);
        (t, root, vec![a, b, c, d], ws)
    }

    #[test]
    fn neighbor_in_a_2x2_grid() {
        let (t, root, l, _) = grid();
        let mut out = Vec::new();
        let gaps = crate::layout::Gaps::default();
        let p = crate::layout::Params {
            area: Rect::new(0.0, 0.0, 1000.0, 800.0),
            gaps: &gaps,
            focused: None,
            accordion_padding: 0.0,
            min: &|_| None,
        };
        crate::layout::compute(&t, root, &p, &mut out);
        let frames = |w: WindowId| out.iter().find(|p| p.window == w).map(|p| p.rect);
        assert_eq!(t.neighbor(l[0], Dir::Right, &frames), Some(l[2]));
        assert_eq!(t.neighbor(l[0], Dir::Down, &frames), Some(l[1]));
        assert_eq!(t.neighbor(l[3], Dir::Left, &frames), Some(l[1]));
        assert_eq!(t.neighbor(l[3], Dir::Up, &frames), Some(l[2]));
        assert_eq!(t.neighbor(l[0], Dir::Left, &frames), None);
        assert_eq!(t.neighbor(l[0], Dir::Up, &frames), None);
    }

    #[test]
    fn neighbor_prefers_orthogonal_overlap_over_tree_order() {
        // h[a, v[b, c]] with a only as tall as c's band: frames decide.
        let ws = wids(3);
        let mut t = Tree::new();
        let root = t.new_root(HTiles);
        let a = leaf(&mut t, root, ws[0]);
        let v = container(&mut t, root, VTiles);
        leaf(&mut t, v, ws[1]);
        let c = leaf(&mut t, v, ws[2]);
        let frames = |w: WindowId| {
            Some(match ws.iter().position(|x| *x == w).unwrap() {
                0 => Rect::new(0.0, 500.0, 100.0, 100.0),
                1 => Rect::new(100.0, 0.0, 100.0, 400.0),
                _ => Rect::new(100.0, 400.0, 100.0, 400.0),
            })
        };
        assert_eq!(t.neighbor(a, Dir::Right, &frames), Some(c));
    }

    #[test]
    fn neighbor_walks_accordion_children_in_order() {
        let ws = wids(3);
        let mut t = Tree::new();
        let root = t.new_root(HAccordion);
        let a = leaf(&mut t, root, ws[0]);
        let b = leaf(&mut t, root, ws[1]);
        let c = leaf(&mut t, root, ws[2]);
        // Overlapping frames, as an accordion produces.
        let frames = |_: WindowId| Some(Rect::new(0.0, 0.0, 1000.0, 800.0));
        assert_eq!(t.neighbor(a, Dir::Right, &frames), Some(b));
        assert_eq!(t.neighbor(b, Dir::Right, &frames), Some(c));
        assert_eq!(t.neighbor(c, Dir::Left, &frames), Some(b));
        assert_eq!(t.neighbor(c, Dir::Right, &frames), None);
    }

    #[test]
    fn dfs_next_and_prev_with_and_without_wrap() {
        let (t, _, l, _) = grid();
        assert_eq!(t.dfs_next(l[0], true, false), Some(l[1]));
        assert_eq!(t.dfs_next(l[1], true, false), Some(l[2]));
        assert_eq!(t.dfs_next(l[3], true, false), None);
        assert_eq!(t.dfs_next(l[3], true, true), Some(l[0]));
        assert_eq!(t.dfs_next(l[0], false, true), Some(l[3]));
        assert_eq!(t.dfs_next(l[0], false, false), None);
        let (t, _, l, _) = row(1);
        assert_eq!(
            t.dfs_next(l[0], true, true),
            None,
            "wrapping onto itself is not a move"
        );
    }

    #[test]
    fn resize_targets_nearest_ancestor_along_the_axis() {
        let (mut t, root, l, _) = grid();
        let v1 = t.parent(l[0]).unwrap();
        let sizes = |_: NodeId| Some(1000.0);
        // Width: a's parent is v (wrong axis), so v1 itself grows inside root.
        assert!(t.resize(l[0], Axis::H, 100.0, &sizes));
        assert!((t.node(v1).weight - 1.2).abs() < 1e-6);
        let v2 = t.children(root)[1];
        assert!((t.node(v2).weight - 0.8).abs() < 1e-6);
        // Height: a itself grows inside v1.
        assert!(t.resize(l[0], Axis::V, -100.0, &sizes));
        assert!((t.node(l[0]).weight - 0.8).abs() < 1e-6);
        assert!((t.node(l[1]).weight - 1.2).abs() < 1e-6);
    }

    #[test]
    fn resize_clamps_to_min_share_and_reports_noop() {
        let (mut t, _, l, _) = row(3);
        let sizes = |_: NodeId| Some(900.0);
        assert!(t.resize(l[0], Axis::H, 10_000.0, &sizes));
        let w: Vec<f64> = l.iter().map(|&x| f64::from(t.node(x).weight)).collect();
        assert!(
            (w[1] - 0.3).abs() < 1e-5 && (w[2] - 0.3).abs() < 1e-5,
            "{w:?}"
        );
        assert!((w.iter().sum::<f64>() - 3.0).abs() < 1e-5);
        assert!(
            !t.resize(l[0], Axis::H, 50.0, &sizes),
            "already at the limit"
        );
        assert!(
            !t.resize(l[0], Axis::V, 50.0, &sizes),
            "no ancestor along V"
        );
    }

    #[test]
    fn flatten_and_balance() {
        let (mut t, root, l, ws) = grid();
        t.nodes[l[0]].weight = 3.0;
        t.balance(root);
        assert!(l.iter().all(|&x| t.node(x).weight == 1.0));
        t.flatten(root);
        assert_eq!(show(&t, root, &ws), "h_tiles[a, b, c, d]");
        assert_eq!(t.node_count(), 5);
        assert!(l.iter().all(|&x| t.parent(x) == Some(root)));
    }

    #[test]
    fn join_with_pairs_leaf_and_sibling_in_opposite_orientation() {
        let (mut t, root, l, ws) = row(3);
        assert!(t.join_with(l[1], Dir::Right));
        assert_eq!(show(&t, root, &ws), "h_tiles[a, v_tiles[b, c]]");
        let (mut t, root, l, ws) = row(3);
        assert!(t.join_with(l[1], Dir::Left));
        assert_eq!(show(&t, root, &ws), "h_tiles[v_tiles[a, b], c]");
        let (mut t, root, l, ws) = row(2);
        assert!(t.join_with(l[0], Dir::Right));
        assert_eq!(show(&t, root, &ws), "v_tiles[a, b]", "root adopts the pair");
        assert!(!t.join_with(l[0], Dir::Up));
    }

    #[test]
    fn swap_leaves_across_parents_keeps_slot_weights() {
        let (mut t, root, l, ws) = grid();
        t.nodes[l[0]].weight = 2.0;
        t.swap_leaves(l[0], l[3]);
        assert_eq!(show(&t, root, &ws), "h_tiles[v_tiles[d, b], v_tiles[c, a]]");
        assert_eq!(t.node(l[3]).weight, 2.0);
        assert_eq!(t.node(l[0]).weight, 1.0);
        assert_eq!(t.parent(l[0]), t.parent(l[2]));
    }

    #[test]
    fn pending_leaf_is_replaced_in_place() {
        let (mut t, root, l, ws) = row(2);
        let p = t.insert_pending(root, Some(l[0]), 7);
        assert_eq!(t.pending_of(p), Some(7));
        assert_eq!(show(&t, root, &ws), "h_tiles[a, ?7, b]");
        t.replace_pending(p, ws[1]);
        assert_eq!(t.window_of(p), Some(ws[1]));
    }

    #[test]
    fn set_layout_on_leaf_targets_parent_and_merges() {
        let ws = wids(3);
        let mut t = Tree::new();
        let root = t.new_root(HTiles);
        leaf(&mut t, root, ws[0]);
        let v = container(&mut t, root, VTiles);
        let b = leaf(&mut t, v, ws[1]);
        leaf(&mut t, v, ws[2]);
        assert_eq!(t.layout_of(b), VTiles);
        t.set_layout(b, VAccordion);
        assert_eq!(show(&t, root, &ws), "h_tiles[a, v_accordion[b, c]]");
        t.set_layout(b, HTiles);
        assert_eq!(show(&t, root, &ws), "h_tiles[a, b, c]");
    }

    #[test]
    fn move_children_merges_trees() {
        let (mut t, root, _, _) = row(2);
        let ws = wids(3);
        let other = t.new_root(HTiles);
        t.insert_window(other, None, ws[2]);
        t.move_children(other, root);
        assert_eq!(show(&t, root, &ws), "h_tiles[a, b, c]");
        t.remove_root(other);
        assert_eq!(t.node_count(), 4);
        t.remove_root(root);
        assert_eq!(t.node_count(), 0);
    }
}
