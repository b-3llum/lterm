//! Split-pane layout: a binary tree of panes, laid out into pixel rectangles.

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl Rect {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x as f64 && y >= self.y as f64 && x < (self.x + self.w) as f64 && y < (self.y + self.h) as f64
    }
}

/// Where a split puts the new pane.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    /// Side by side (vertical divider).
    Right,
    /// Stacked (horizontal divider).
    Down,
}

pub enum Node {
    Leaf(usize),
    Split { dir: Dir, ratio: f32, a: Box<Node>, b: Box<Node> },
}

/// Divide `r` into the two halves of a split, leaving `gap` pixels for the divider.
pub fn split_rect(r: Rect, dir: Dir, ratio: f32, gap: usize) -> (Rect, Rect, Rect) {
    match dir {
        Dir::Right => {
            let wa = (r.w.saturating_sub(gap) as f32 * ratio).round() as usize;
            let a = Rect { w: wa, ..r };
            let div = Rect { x: r.x + wa, w: gap.min(r.w - wa), ..r };
            let b = Rect { x: r.x + wa + gap, w: r.w.saturating_sub(wa + gap), ..r };
            (a, b, div)
        }
        Dir::Down => {
            let ha = (r.h.saturating_sub(gap) as f32 * ratio).round() as usize;
            let a = Rect { h: ha, ..r };
            let div = Rect { y: r.y + ha, h: gap.min(r.h - ha), ..r };
            let b = Rect { y: r.y + ha + gap, h: r.h.saturating_sub(ha + gap), ..r };
            (a, b, div)
        }
    }
}

impl Node {
    /// Replace pane `target` with a split of it and pane `new`.
    pub fn split(&mut self, target: usize, dir: Dir, new: usize) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                *self = Node::Split { dir, ratio: 0.5, a: Box::new(Node::Leaf(target)), b: Box::new(Node::Leaf(new)) };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => a.split(target, dir, new) || b.split(target, dir, new),
        }
    }

    /// Remove pane `target`; its sibling takes its parent's place. None if no panes remain.
    pub fn remove(self, target: usize) -> Option<Node> {
        self.retain(&|id| id != target)
    }

    /// Keep only the panes for which `keep` is true.
    pub fn retain(self, keep: &dyn Fn(usize) -> bool) -> Option<Node> {
        match self {
            Node::Leaf(id) => keep(id).then_some(Node::Leaf(id)),
            Node::Split { dir, ratio, a, b } => match (a.retain(keep), b.retain(keep)) {
                (Some(a), Some(b)) => Some(Node::Split { dir, ratio, a: Box::new(a), b: Box::new(b) }),
                (one, None) | (None, one) => one,
            },
        }
    }

    pub fn leaves(&self) -> Vec<usize> {
        match self {
            Node::Leaf(id) => vec![*id],
            Node::Split { a, b, .. } => {
                let mut v = a.leaves();
                v.extend(b.leaves());
                v
            }
        }
    }

    pub fn contains(&self, id: usize) -> bool {
        match self {
            Node::Leaf(x) => *x == id,
            Node::Split { a, b, .. } => a.contains(id) || b.contains(id),
        }
    }

    /// Compact text form, e.g. `r0.500(1)(d0.500(2)(3))`.
    pub fn encode(&self, out: &mut String) {
        match self {
            Node::Leaf(id) => out.push_str(&id.to_string()),
            Node::Split { dir, ratio, a, b } => {
                out.push(if *dir == Dir::Right { 'r' } else { 'd' });
                out.push_str(&format!("{ratio:.3}("));
                a.encode(out);
                out.push_str(")(");
                b.encode(out);
                out.push(')');
            }
        }
    }

    pub fn decode(s: &str) -> Option<Node> {
        let (node, rest) = parse_node(s.as_bytes())?;
        rest.is_empty().then_some(node)
    }

    pub fn first_leaf(&self) -> usize {
        match self {
            Node::Leaf(id) => *id,
            Node::Split { a, .. } => a.first_leaf(),
        }
    }

    /// Move the nearest divider along `axis` that bounds pane `target`.
    /// Returns None if `target` isn't in this subtree, Some(done) otherwise.
    pub fn resize(&mut self, target: usize, axis: Dir, delta: f32) -> Option<bool> {
        match self {
            Node::Leaf(id) => (*id == target).then_some(false),
            Node::Split { dir, ratio, a, b } => {
                let done = a.resize(target, axis, delta).or_else(|| b.resize(target, axis, delta))?;
                if !done && *dir == axis {
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    return Some(true);
                }
                Some(done)
            }
        }
    }

    pub fn layout(&self, r: Rect, gap: usize, panes: &mut Vec<(usize, Rect)>, dividers: &mut Vec<Rect>) {
        match self {
            Node::Leaf(id) => panes.push((*id, r)),
            Node::Split { dir, ratio, a, b } => {
                let (ra, rb, div) = split_rect(r, *dir, *ratio, gap);
                dividers.push(div);
                a.layout(ra, gap, panes, dividers);
                b.layout(rb, gap, panes, dividers);
            }
        }
    }
}

fn parse_node(s: &[u8]) -> Option<(Node, &[u8])> {
    match *s.first()? {
        c @ (b'r' | b'd') => {
            let dir = if c == b'r' { Dir::Right } else { Dir::Down };
            let s = &s[1..];
            let open = s.iter().position(|&b| b == b'(')?;
            let ratio: f32 = std::str::from_utf8(&s[..open]).ok()?.parse().ok()?;
            let (a, s) = parse_node(&s[open + 1..])?;
            let (b, s) = parse_node(s.strip_prefix(&b")("[..])?)?;
            let s = s.strip_prefix(&b")"[..])?;
            Some((Node::Split { dir, ratio: ratio.clamp(0.1, 0.9), a: Box::new(a), b: Box::new(b) }, s))
        }
        b'0'..=b'9' => {
            let end = s.iter().position(|b| !b.is_ascii_digit()).unwrap_or(s.len());
            let id = std::str::from_utf8(&s[..end]).ok()?.parse().ok()?;
            Some((Node::Leaf(id), &s[end..]))
        }
        _ => None,
    }
}

/// A tab: its own split layout, focused pane and zoom state.
pub struct Tab {
    pub tree: Node,
    pub focus: usize,
    pub zoomed: bool,
}

/// Serialize tabs for a session server to keep while no window is attached.
pub fn encode_tabs(tabs: &[Tab], active: usize) -> String {
    let mut s = format!("active {active}\n");
    for t in tabs {
        let mut tree = String::new();
        t.tree.encode(&mut tree);
        s.push_str(&format!("{} {} {tree}\n", t.focus, t.zoomed as u8));
    }
    s
}

pub fn decode_tabs(s: &str) -> (Vec<Tab>, usize) {
    let (mut tabs, mut active) = (Vec::new(), 0);
    for line in s.lines() {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next(), parts.next()) {
            (Some("active"), Some(n), None) => active = n.parse().unwrap_or(0),
            (Some(focus), Some(zoomed), Some(tree)) => {
                if let (Ok(focus), Some(tree)) = (focus.parse(), Node::decode(tree)) {
                    tabs.push(Tab { tree, focus, zoomed: zoomed == "1" });
                }
            }
            _ => {}
        }
    }
    (tabs, active)
}

/// The pane adjacent to `from` in direction (dx, dy), preferring the closest one.
pub fn neighbor(rects: &[(usize, Rect)], from: usize, dx: i32, dy: i32) -> Option<usize> {
    let (_, cur) = rects.iter().find(|(id, _)| *id == from)?;
    let overlap = |a: usize, al: usize, b: usize, bl: usize| a < b + bl && b < a + al;
    let center = |r: &Rect| ((r.x + r.w / 2) as i64, (r.y + r.h / 2) as i64);
    let (cx, cy) = center(cur);
    rects
        .iter()
        .filter(|(id, r)| {
            *id != from
                && match (dx, dy) {
                    (-1, _) => r.x + r.w <= cur.x && overlap(r.y, r.h, cur.y, cur.h),
                    (1, _) => r.x >= cur.x + cur.w && overlap(r.y, r.h, cur.y, cur.h),
                    (_, -1) => r.y + r.h <= cur.y && overlap(r.x, r.w, cur.x, cur.w),
                    _ => r.y >= cur.y + cur.h && overlap(r.x, r.w, cur.x, cur.w),
                }
        })
        .min_by_key(|(_, r)| {
            let (x, y) = center(r);
            (x - cx).abs() + (y - cy).abs()
        })
        .map(|(id, _)| *id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lay(n: &Node) -> Vec<(usize, Rect)> {
        let (mut p, mut d) = (Vec::new(), Vec::new());
        n.layout(Rect { x: 0, y: 0, w: 101, h: 50 }, 1, &mut p, &mut d);
        p
    }

    #[test]
    fn split_navigate_resize_remove() {
        let mut t = Node::Leaf(0);
        assert!(t.split(0, Dir::Right, 1));
        assert!(t.split(1, Dir::Down, 2));
        let r = lay(&t);
        assert_eq!(r[0], (0, Rect { x: 0, y: 0, w: 50, h: 50 }));
        assert_eq!(r[1].1.x, 51);
        assert_eq!(neighbor(&r, 0, 1, 0), Some(1)); // 1 and 2 are equally close; first wins
        assert_eq!(neighbor(&r, 1, 0, 1), Some(2));
        assert_eq!(neighbor(&r, 2, -1, 0), Some(0));
        assert_eq!(neighbor(&r, 0, -1, 0), None);

        assert_eq!(t.resize(2, Dir::Right, 0.1), Some(true));
        assert_eq!(lay(&t)[0].1.w, 60);

        let t = t.remove(1).unwrap();
        let r = lay(&t);
        assert_eq!(r.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![0, 2]);
        assert!(t.remove(0).unwrap().remove(2).is_none());
    }

    #[test]
    fn tabs_round_trip() {
        let mut tree = Node::Leaf(4);
        tree.split(4, Dir::Right, 7);
        tree.split(7, Dir::Down, 12);
        let tabs = vec![Tab { tree, focus: 7, zoomed: true }, Tab { tree: Node::Leaf(9), focus: 9, zoomed: false }];
        let text = encode_tabs(&tabs, 1);
        let (back, active) = decode_tabs(&text);
        assert_eq!(active, 1);
        assert_eq!(back.len(), 2);
        assert_eq!((back[0].focus, back[0].zoomed, back[0].tree.leaves()), (7, true, vec![4, 7, 12]));
        assert_eq!(encode_tabs(&back, 1), text);
        assert!(Node::decode("r0.5(1)(").is_none());
    }
}
