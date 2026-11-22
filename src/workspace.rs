//! Pane layout: a binary tree of splits whose leaves are panes, each with its
//! own tab bar (one session per tab). Pure data — no GPU, no PTY — so every
//! operation is unit-tested.

use crate::session::SessionId;

pub type PaneId = u64;

/// Axis-aligned rectangle (in whatever unit the caller uses).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDir {
    /// Side by side (a vertical divider).
    Vertical,
    /// Stacked (a horizontal divider).
    Horizontal,
}

/// Smallest share a pane can be resized down to.
const MIN_SHARE: f32 = 0.05;

#[derive(Clone, Debug, PartialEq)]
enum Node {
    Leaf(PaneId),
    /// Any number of children laid out along `dir`; `sizes` are their shares
    /// of the space (summing to 1).
    Split { dir: SplitDir, children: Vec<Node>, sizes: Vec<f32> },
}

/// A draggable boundary between children `index` and `index + 1` of the
/// split at `path` (child indices from the root).
#[derive(Clone, Debug, PartialEq)]
pub struct Divider {
    pub path: Vec<usize>,
    pub index: usize,
    pub dir: SplitDir,
    /// The gap between the two children.
    pub rect: Rect,
    /// Space shared by the split's children (gaps excluded), to turn a
    /// pixel delta into a share.
    pub extent: f32,
}

/// Where a dragged tab lands.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DropTarget {
    /// Into `pane`'s tab bar at `index`.
    Tab { pane: PaneId, index: usize },
    /// Into a new pane beside `pane` (`before` = left of / above it).
    Split { pane: PaneId, dir: SplitDir, before: bool },
}

impl Node {
    /// Builds a split, collapsing a single child and merging children that
    /// split the same way (so N panes side by side share one level).
    fn split(dir: SplitDir, children: Vec<Node>, sizes: Vec<f32>) -> Option<Node> {
        let total: f32 = sizes.iter().sum();
        let mut flat_c = Vec::with_capacity(children.len());
        let mut flat_s = Vec::with_capacity(children.len());
        for (child, size) in children.into_iter().zip(sizes) {
            let share = if total > 0.0 { size / total } else { 0.0 };
            match child {
                Node::Split { dir: d, children: inner, sizes: inner_sizes } if d == dir => {
                    for (c, s) in inner.into_iter().zip(inner_sizes) {
                        flat_c.push(c);
                        flat_s.push(s * share);
                    }
                }
                other => {
                    flat_c.push(other);
                    flat_s.push(share);
                }
            }
        }
        match flat_c.len() {
            0 => None,
            1 => flat_c.pop(),
            n => {
                let sum: f32 = flat_s.iter().sum();
                let sizes = if sum > 0.0 { flat_s.iter().map(|s| s / sum).collect() } else { vec![1.0 / n as f32; n] };
                Some(Node::Split { dir, children: flat_c, sizes })
            }
        }
    }

    fn leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { children, .. } => children.iter().for_each(|c| c.leaves(out)),
        }
    }

    /// Puts leaf `new` next to leaf `target`. Splitting the same way as the
    /// parent adds a sibling (one more equal share) instead of nesting.
    fn insert_beside(&mut self, target: PaneId, new: PaneId, dir: SplitDir, before: bool) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                let mut children = vec![Node::Leaf(target), Node::Leaf(new)];
                if before {
                    children.reverse();
                }
                *self = Node::Split { dir, children, sizes: vec![0.5, 0.5] };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { dir: d, children, sizes } => {
                if *d == dir
                    && let Some(i) = children.iter().position(|c| *c == Node::Leaf(target))
                {
                    let n = children.len() as f32 + 1.0;
                    sizes.iter_mut().for_each(|s| *s *= (n - 1.0) / n);
                    let at = if before { i } else { i + 1 };
                    children.insert(at, Node::Leaf(new));
                    sizes.insert(at, 1.0 / n);
                    return true;
                }
                children.iter_mut().any(|c| c.insert_beside(target, new, dir, before))
            }
        }
    }

    /// Removes leaf `target`; its siblings take the space. `None` if the
    /// tree becomes empty.
    fn remove_leaf(self, target: PaneId) -> Option<Node> {
        match self {
            Node::Leaf(id) if id == target => None,
            leaf @ Node::Leaf(_) => Some(leaf),
            Node::Split { dir, children, sizes } => {
                let (children, sizes): (Vec<_>, Vec<_>) =
                    children.into_iter().zip(sizes).filter_map(|(c, s)| Some((c.remove_leaf(target)?, s))).unzip();
                Node::split(dir, children, sizes)
            }
        }
    }

    fn at_path_mut(&mut self, path: &[usize]) -> Option<&mut Node> {
        match path.split_first() {
            None => Some(self),
            Some((i, rest)) => match self {
                Node::Split { children, .. } => children.get_mut(*i)?.at_path_mut(rest),
                Node::Leaf(_) => None,
            },
        }
    }

    fn layout(&self, rect: Rect, gap: f32, path: &mut Vec<usize>, panes: &mut Vec<(PaneId, Rect)>, dividers: &mut Vec<Divider>) {
        match self {
            Node::Leaf(id) => panes.push((*id, rect)),
            Node::Split { dir, children, sizes } => {
                let n = children.len();
                let (start, extent) = match dir {
                    SplitDir::Vertical => (rect.x, rect.w),
                    SplitDir::Horizontal => (rect.y, rect.h),
                };
                let avail = (extent - gap * (n.saturating_sub(1)) as f32).max(0.0);
                let mut pos = start;
                let mut used = 0.0;
                for (i, (child, share)) in children.iter().zip(sizes).enumerate() {
                    let len = if i + 1 == n { avail - used } else { (avail * share).round().min(avail - used) };
                    used += len;
                    let r = match dir {
                        SplitDir::Vertical => Rect::new(pos, rect.y, len, rect.h),
                        SplitDir::Horizontal => Rect::new(rect.x, pos, rect.w, len),
                    };
                    path.push(i);
                    child.layout(r, gap, path, panes, dividers);
                    path.pop();
                    pos += len;
                    if i + 1 < n {
                        let rect = match dir {
                            SplitDir::Vertical => Rect::new(pos, rect.y, gap, rect.h),
                            SplitDir::Horizontal => Rect::new(rect.x, pos, rect.w, gap),
                        };
                        dividers.push(Divider { path: path.clone(), index: i, dir: *dir, rect, extent: avail });
                    }
                    pos += gap;
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pane {
    pub id: PaneId,
    pub tabs: Vec<SessionId>,
    pub active: usize,
}

impl Pane {
    pub fn active_session(&self) -> Option<SessionId> {
        self.tabs.get(self.active).copied()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Workspace {
    root: Option<Node>,
    panes: Vec<Pane>,
    focused: Option<PaneId>,
    next_pane: PaneId,
}

impl Workspace {
    pub fn new() -> Self {
        Self { next_pane: 1, ..Default::default() }
    }

    fn new_pane(&mut self, session: SessionId) -> PaneId {
        let id = self.next_pane;
        self.next_pane += 1;
        self.panes.push(Pane { id, tabs: vec![session], active: 0 });
        id
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }

    fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id == id)
    }

    /// Panes in visual order (left→right, top→bottom).
    pub fn pane_order(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.leaves(&mut out);
        }
        out
    }

    pub fn focused_pane(&self) -> Option<PaneId> {
        self.focused
    }

    pub fn focused_session(&self) -> Option<SessionId> {
        self.focused.and_then(|id| self.pane(id)).and_then(Pane::active_session)
    }

    pub fn pane_of(&self, session: SessionId) -> Option<PaneId> {
        self.panes.iter().find(|p| p.tabs.contains(&session)).map(|p| p.id)
    }

    /// Sessions currently visible (the active tab of every pane).
    pub fn visible_sessions(&self) -> Vec<SessionId> {
        self.pane_order().into_iter().filter_map(|id| self.pane(id)?.active_session()).collect()
    }

    /// Opens `session` as a new tab of the focused pane (creating the first
    /// pane if the workspace is empty) and focuses it.
    pub fn add_session(&mut self, session: SessionId) {
        match self.focused.filter(|id| self.pane(*id).is_some()) {
            Some(id) => {
                let pane = self.pane_mut(id).expect("focused pane exists");
                pane.tabs.push(session);
                pane.active = pane.tabs.len() - 1;
            }
            None => {
                let id = self.new_pane(session);
                self.root = Some(Node::Leaf(id));
                self.focused = Some(id);
            }
        }
    }

    /// Splits the focused pane, putting `session` in the new pane (right of
    /// or below the current one), and focuses it. Splitting again the same
    /// way adds another pane beside it: there is no limit.
    pub fn split(&mut self, dir: SplitDir, session: SessionId) -> PaneId {
        let Some(target) = self.focused.filter(|id| self.pane(*id).is_some()) else {
            self.add_session(session);
            return self.focused.expect("pane created");
        };
        self.split_beside(target, dir, false, session)
    }

    fn split_beside(&mut self, target: PaneId, dir: SplitDir, before: bool, session: SessionId) -> PaneId {
        let new_id = self.new_pane(session);
        if let Some(root) = &mut self.root {
            root.insert_beside(target, new_id, dir, before);
        }
        self.focused = Some(new_id);
        new_id
    }

    /// Removes a session's tab; empty panes disappear and their siblings take
    /// the space. Returns whether the session was shown anywhere.
    pub fn remove_session(&mut self, session: SessionId) -> bool {
        let Some(pane_id) = self.pane_of(session) else { return false };
        let order_before = self.pane_order();
        let pane = self.pane_mut(pane_id).expect("pane exists");
        let index = pane.tabs.iter().position(|s| *s == session).expect("tab exists");
        pane.tabs.remove(index);
        if index < pane.active || pane.active >= pane.tabs.len() {
            pane.active = pane.active.saturating_sub(1);
        }
        if pane.tabs.is_empty() {
            self.panes.retain(|p| p.id != pane_id);
            self.root = self.root.take().and_then(|r| r.remove_leaf(pane_id));
            if self.focused == Some(pane_id) {
                // Focus the neighbour that came before (or after) it.
                let pos = order_before.iter().position(|p| *p == pane_id).unwrap_or(0);
                let order = self.pane_order();
                self.focused = order.get(pos.saturating_sub(1).min(order.len().saturating_sub(1))).copied();
            }
        }
        true
    }

    /// Moves a tab (drag and drop): into a pane's tab bar at an index
    /// (reordering within its own pane), or into a new pane beside one. The
    /// moved tab ends up active and focused. Returns whether anything moved.
    pub fn move_tab(&mut self, session: SessionId, target: DropTarget) -> bool {
        let Some(from) = self.pane_of(session) else { return false };
        let from_len = self.pane(from).map_or(0, |p| p.tabs.len());
        match target {
            DropTarget::Tab { pane, index } => {
                if self.pane(pane).is_none() {
                    return false;
                }
                if pane == from {
                    let p = self.pane_mut(pane).expect("pane exists");
                    let old = p.tabs.iter().position(|s| *s == session).expect("tab exists");
                    let new = if index > old { index - 1 } else { index }.min(p.tabs.len() - 1);
                    let tab = p.tabs.remove(old);
                    p.tabs.insert(new, tab);
                    p.active = new;
                    self.focused = Some(pane);
                    return old != new;
                }
                self.remove_session(session);
                let p = self.pane_mut(pane).expect("target pane survives: it is not the source");
                let index = index.min(p.tabs.len());
                p.tabs.insert(index, session);
                p.active = index;
                self.focused = Some(pane);
                true
            }
            DropTarget::Split { pane, dir, before } => {
                // A pane's only tab can't be split off from itself.
                if self.pane(pane).is_none() || (pane == from && from_len == 1) {
                    return false;
                }
                self.remove_session(session);
                self.split_beside(pane, dir, before, session);
                true
            }
        }
    }

    /// Moves the boundary after child `index` of the split at `path` by
    /// `delta` (a share of the split's space), keeping both sides at least
    /// `min` (a share too).
    pub fn resize_split(&mut self, path: &[usize], index: usize, delta: f32, min: f32) {
        let Some(Node::Split { sizes, .. }) = self.root.as_mut().and_then(|r| r.at_path_mut(path)) else { return };
        if index + 1 >= sizes.len() {
            return;
        }
        let min = min.clamp(MIN_SHARE, 0.45);
        let pair = sizes[index] + sizes[index + 1];
        let a = (sizes[index] + delta).clamp(min.min(pair / 2.0), (pair - min).max(pair / 2.0));
        sizes[index] = a;
        sizes[index + 1] = pair - a;
    }

    /// Resets every split to equal shares.
    pub fn equalize(&mut self) {
        fn walk(node: &mut Node) {
            if let Node::Split { children, sizes, .. } = node {
                let n = sizes.len() as f32;
                sizes.iter_mut().for_each(|s| *s = 1.0 / n);
                children.iter_mut().for_each(walk);
            }
        }
        if let Some(root) = &mut self.root {
            walk(root);
        }
    }

    /// Focuses the pane showing `session` and activates its tab.
    pub fn focus_session(&mut self, session: SessionId) -> bool {
        let Some(pane_id) = self.pane_of(session) else { return false };
        let pane = self.pane_mut(pane_id).expect("pane exists");
        pane.active = pane.tabs.iter().position(|s| *s == session).expect("tab exists");
        self.focused = Some(pane_id);
        true
    }

    pub fn focus_pane(&mut self, pane: PaneId) {
        if self.pane(pane).is_some() {
            self.focused = Some(pane);
        }
    }

    pub fn activate_tab(&mut self, pane: PaneId, index: usize) {
        if let Some(p) = self.pane_mut(pane)
            && index < p.tabs.len()
        {
            p.active = index;
            self.focused = Some(pane);
        }
    }

    /// Cycles tabs of the focused pane (`delta` = ±1).
    pub fn cycle_tab(&mut self, delta: isize) {
        let Some(id) = self.focused else { return };
        if let Some(p) = self.pane_mut(id)
            && !p.tabs.is_empty()
        {
            let n = p.tabs.len() as isize;
            p.active = ((p.active as isize + delta).rem_euclid(n)) as usize;
        }
    }

    pub fn focus_next_pane(&mut self) {
        let order = self.pane_order();
        if order.is_empty() {
            return;
        }
        let pos = self.focused.and_then(|f| order.iter().position(|p| *p == f)).unwrap_or(0);
        self.focused = Some(order[(pos + 1) % order.len()]);
    }

    /// Pane rectangles for the given area, `gap` between siblings.
    #[cfg(test)]
    pub fn layout(&self, area: Rect, gap: f32) -> Vec<(PaneId, Rect)> {
        self.layout_with_dividers(area, gap).0
    }

    /// Pane rectangles plus the draggable boundaries between them.
    pub fn layout_with_dividers(&self, area: Rect, gap: f32) -> (Vec<(PaneId, Rect)>, Vec<Divider>) {
        let (mut panes, mut dividers) = (Vec::new(), Vec::new());
        if let Some(root) = &self.root {
            root.layout(area, gap, &mut Vec::new(), &mut panes, &mut dividers);
        }
        (panes, dividers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_session_creates_root_pane() {
        let mut ws = Workspace::new();
        assert!(ws.is_empty());
        ws.add_session(10);
        assert_eq!(ws.focused_session(), Some(10));
        ws.add_session(11);
        let pane = ws.pane(ws.focused_pane().unwrap()).unwrap();
        assert_eq!(pane.tabs, [10, 11]);
        assert_eq!(ws.focused_session(), Some(11));
    }

    #[test]
    fn vertical_split_50_50_layout() {
        let mut ws = Workspace::new();
        ws.add_session(1);
        let right = ws.split(SplitDir::Vertical, 2);
        assert_eq!(ws.focused_pane(), Some(right));
        let rects = ws.layout(Rect::new(0.0, 0.0, 1001.0, 600.0), 1.0);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].1, Rect::new(0.0, 0.0, 500.0, 600.0));
        assert_eq!(rects[1].1, Rect::new(501.0, 0.0, 500.0, 600.0));
        assert_eq!(ws.visible_sessions(), [1, 2]);
    }

    #[test]
    fn nested_splits_and_focus_cycle() {
        let mut ws = Workspace::new();
        ws.add_session(1);
        ws.split(SplitDir::Vertical, 2);
        ws.split(SplitDir::Horizontal, 3);
        let rects = ws.layout(Rect::new(0.0, 0.0, 100.0, 100.0), 0.0);
        assert_eq!(rects.iter().map(|r| r.1).collect::<Vec<_>>(), [
            Rect::new(0.0, 0.0, 50.0, 100.0),
            Rect::new(50.0, 0.0, 50.0, 50.0),
            Rect::new(50.0, 50.0, 50.0, 50.0),
        ]);
        assert_eq!(ws.focused_session(), Some(3));
        ws.focus_next_pane();
        assert_eq!(ws.focused_session(), Some(1));
        ws.focus_next_pane();
        assert_eq!(ws.focused_session(), Some(2));
    }

    #[test]
    fn closing_last_tab_collapses_pane() {
        let mut ws = Workspace::new();
        ws.add_session(1);
        ws.split(SplitDir::Vertical, 2);
        ws.add_session(3); // second tab in the right pane
        assert!(ws.remove_session(3));
        assert_eq!(ws.focused_session(), Some(2));
        assert!(ws.remove_session(2));
        // Right pane gone: the left one takes the whole area and the focus.
        assert_eq!(ws.layout(Rect::new(0.0, 0.0, 10.0, 10.0), 1.0), [(1, Rect::new(0.0, 0.0, 10.0, 10.0))]);
        assert_eq!(ws.focused_session(), Some(1));
        assert!(ws.remove_session(1));
        assert!(ws.is_empty());
        assert_eq!(ws.focused_session(), None);
        assert!(!ws.remove_session(42));
    }

    #[test]
    fn removing_a_tab_keeps_a_sensible_active_tab() {
        let mut ws = Workspace::new();
        for s in 1..=4 {
            ws.add_session(s);
        }
        ws.focus_session(2);
        ws.remove_session(1); // before the active one → index shifts
        assert_eq!(ws.focused_session(), Some(2));
        ws.focus_session(4);
        ws.remove_session(4); // the active, last one → previous
        assert_eq!(ws.focused_session(), Some(3));
    }

    #[test]
    fn tab_cycling_and_focus_by_session() {
        let mut ws = Workspace::new();
        ws.add_session(1);
        ws.add_session(2);
        ws.split(SplitDir::Vertical, 3);
        assert!(ws.focus_session(1));
        assert_eq!(ws.focused_session(), Some(1));
        ws.cycle_tab(1);
        assert_eq!(ws.focused_session(), Some(2));
        ws.cycle_tab(1);
        assert_eq!(ws.focused_session(), Some(1));
        ws.cycle_tab(-1);
        assert_eq!(ws.focused_session(), Some(2));
        assert!(!ws.focus_session(99));
        let left = ws.pane_of(1).unwrap();
        ws.activate_tab(left, 0);
        assert_eq!(ws.focused_session(), Some(1));
    }

    #[test]
    fn split_on_empty_workspace_just_adds() {
        let mut ws = Workspace::new();
        ws.split(SplitDir::Horizontal, 7);
        assert_eq!(ws.visible_sessions(), [7]);
    }
    #[test]
    fn repeated_splits_share_one_level_equally() {
        let mut ws = Workspace::new();
        ws.add_session(1);
        for s in 2..=5 {
            ws.split(SplitDir::Vertical, s);
        }
        let rects = ws.layout(Rect::new(0.0, 0.0, 500.0, 100.0), 0.0);
        assert_eq!(rects.iter().map(|r| r.1.w).collect::<Vec<_>>(), [100.0; 5]);
        assert_eq!(ws.visible_sessions(), [1, 2, 3, 4, 5]);
        // Closing one gives its space back to the others, still one level.
        ws.remove_session(3);
        let rects = ws.layout(Rect::new(0.0, 0.0, 400.0, 100.0), 0.0);
        assert_eq!(rects.iter().map(|r| r.1.w).collect::<Vec<_>>(), [100.0; 4]);
    }

    #[test]
    fn dividers_resize_neighbours() {
        let mut ws = Workspace::new();
        ws.add_session(1);
        ws.split(SplitDir::Vertical, 2);
        ws.split(SplitDir::Vertical, 3);
        let (_, dividers) = ws.layout_with_dividers(Rect::new(0.0, 0.0, 302.0, 10.0), 1.0);
        assert_eq!(dividers.len(), 2);
        assert_eq!(dividers[0].rect, Rect::new(100.0, 0.0, 1.0, 10.0));
        let d = dividers[0].clone();
        ws.resize_split(&d.path, d.index, 50.0 / d.extent, 0.1);
        let rects = ws.layout(Rect::new(0.0, 0.0, 302.0, 10.0), 1.0);
        assert_eq!(rects.iter().map(|r| r.1.w).collect::<Vec<_>>(), [150.0, 50.0, 100.0]);
        // Can't shrink a pane below the minimum.
        ws.resize_split(&d.path, d.index, 10.0, 0.1);
        let rects = ws.layout(Rect::new(0.0, 0.0, 300.0, 10.0), 0.0);
        assert_eq!(rects[1].1.w, 30.0);
        ws.equalize();
        let rects = ws.layout(Rect::new(0.0, 0.0, 300.0, 10.0), 0.0);
        assert_eq!(rects.iter().map(|r| r.1.w).collect::<Vec<_>>(), [100.0; 3]);
    }

    #[test]
    fn reorder_tabs_within_a_pane() {
        let mut ws = Workspace::new();
        for s in 1..=3 {
            ws.add_session(s);
        }
        let pane = ws.focused_pane().unwrap();
        assert!(ws.move_tab(1, DropTarget::Tab { pane, index: 3 }));
        assert_eq!(ws.pane(pane).unwrap().tabs, [2, 3, 1]);
        assert_eq!(ws.focused_session(), Some(1));
        assert!(ws.move_tab(1, DropTarget::Tab { pane, index: 0 }));
        assert_eq!(ws.pane(pane).unwrap().tabs, [1, 2, 3]);
        assert!(!ws.move_tab(1, DropTarget::Tab { pane, index: 1 }));
    }

    #[test]
    fn move_tabs_between_panes_and_into_splits() {
        let mut ws = Workspace::new();
        ws.add_session(1);
        ws.add_session(2);
        let left = ws.focused_pane().unwrap();
        // Drag tab 2 to the right edge: a new pane.
        assert!(ws.move_tab(2, DropTarget::Split { pane: left, dir: SplitDir::Vertical, before: false }));
        let right = ws.pane_of(2).unwrap();
        assert_ne!(left, right);
        assert_eq!(ws.visible_sessions(), [1, 2]);
        // A pane's only tab can't split off from itself.
        assert!(!ws.move_tab(2, DropTarget::Split { pane: right, dir: SplitDir::Vertical, before: true }));
        // Drag it back into the left tab bar: the right pane disappears.
        assert!(ws.move_tab(2, DropTarget::Tab { pane: left, index: 0 }));
        assert_eq!(ws.pane_order(), [left]);
        assert_eq!(ws.pane(left).unwrap().tabs, [2, 1]);
        assert_eq!(ws.focused_session(), Some(2));
        // Moving the last tab of a pane next to another pane.
        ws.split(SplitDir::Vertical, 3);
        let third = ws.pane_of(3).unwrap();
        assert!(ws.move_tab(3, DropTarget::Split { pane: left, dir: SplitDir::Horizontal, before: true }));
        assert!(ws.pane(third).is_none());
        let rects = ws.layout(Rect::new(0.0, 0.0, 10.0, 10.0), 0.0);
        assert_eq!(rects.iter().map(|r| r.1).collect::<Vec<_>>(), [Rect::new(0.0, 0.0, 10.0, 5.0), Rect::new(0.0, 5.0, 10.0, 5.0)]);
        assert_eq!(ws.visible_sessions(), [3, 2]);
    }
}
