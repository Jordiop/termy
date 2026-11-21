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

#[derive(Clone, Debug, PartialEq)]
enum Node {
    Leaf(PaneId),
    Split { dir: SplitDir, ratio: f32, first: Box<Node>, second: Box<Node> },
}

impl Node {
    fn leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { first, second, .. } => {
                first.leaves(out);
                second.leaves(out);
            }
        }
    }

    /// Replaces leaf `target` with `with`. Returns whether it was found.
    fn replace_leaf(&mut self, target: PaneId, with: &mut Option<Node>) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                if let Some(node) = with.take() {
                    *self = node;
                }
                true
            }
            Node::Leaf(_) => false,
            Node::Split { first, second, .. } => first.replace_leaf(target, with) || second.replace_leaf(target, with),
        }
    }

    /// Removes leaf `target`, collapsing its parent split. `None` if the tree
    /// becomes empty.
    fn remove_leaf(self, target: PaneId) -> Option<Node> {
        match self {
            Node::Leaf(id) if id == target => None,
            leaf @ Node::Leaf(_) => Some(leaf),
            Node::Split { dir, ratio, first, second } => match (first.remove_leaf(target), second.remove_leaf(target)) {
                (Some(a), Some(b)) => Some(Node::Split { dir, ratio, first: Box::new(a), second: Box::new(b) }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }

    fn layout(&self, rect: Rect, gap: f32, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            Node::Leaf(id) => out.push((*id, rect)),
            Node::Split { dir, ratio, first, second } => {
                let (a, b) = match dir {
                    SplitDir::Vertical => {
                        let w1 = ((rect.w - gap) * ratio).round();
                        (
                            Rect::new(rect.x, rect.y, w1, rect.h),
                            Rect::new(rect.x + w1 + gap, rect.y, rect.w - w1 - gap, rect.h),
                        )
                    }
                    SplitDir::Horizontal => {
                        let h1 = ((rect.h - gap) * ratio).round();
                        (
                            Rect::new(rect.x, rect.y, rect.w, h1),
                            Rect::new(rect.x, rect.y + h1 + gap, rect.w, rect.h - h1 - gap),
                        )
                    }
                };
                first.layout(a, gap, out);
                second.layout(b, gap, out);
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
    /// or below the current one), and focuses it.
    pub fn split(&mut self, dir: SplitDir, session: SessionId) -> PaneId {
        let Some(target) = self.focused.filter(|id| self.pane(*id).is_some()) else {
            self.add_session(session);
            return self.focused.expect("pane created");
        };
        let new_id = self.new_pane(session);
        let mut replacement = Some(Node::Split {
            dir,
            ratio: 0.5,
            first: Box::new(Node::Leaf(target)),
            second: Box::new(Node::Leaf(new_id)),
        });
        if let Some(root) = &mut self.root {
            root.replace_leaf(target, &mut replacement);
        }
        self.focused = Some(new_id);
        new_id
    }

    /// Removes a session's tab; empty panes disappear and their sibling takes
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
    pub fn layout(&self, area: Rect, gap: f32) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.layout(area, gap, &mut out);
        }
        out
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
}
