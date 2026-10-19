//! What the window shows about the coding agents in its panes, and which one
//! "next agent" goes to. Plain data in and out, so it is tested without a window.

use crate::control::{Agent, AgentState};

pub const WORKING: u32 = 0xe0af68;
pub const BLOCKED: u32 = 0xf7768e;
pub const DONE: u32 = 0x9ece6a;
pub const QUIET: u32 = 0x565f89;

/// How an agent shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Look {
    pub color: u32,
    /// Its pane gets a border.
    pub border: bool,
    /// Blocked 3, unseen done 2, working 1, else 0.
    pub urgency: u8,
}

/// How `agent` shows; `seen`: its latest "done" has been on screen in the focused pane.
pub fn look(agent: Option<&Agent>, seen: bool) -> Option<Look> {
    let (color, border, urgency) = match agent?.state {
        AgentState::Blocked => (BLOCKED, true, 3),
        AgentState::Done if !seen => (DONE, true, 2),
        AgentState::Working => (WORKING, true, 1),
        AgentState::Done | AgentState::Idle => (QUIET, false, 0),
    };
    Some(Look { color, border, urgency })
}

/// One pane as seen by these decisions.
#[derive(Clone, Copy)]
pub struct Seat<'a> {
    pub id: usize,
    pub agent: Option<&'a Agent>,
    pub seen: bool,
    pub title: &'a str,
}

impl Seat<'_> {
    pub fn look(&self) -> Option<Look> {
        look(self.agent, self.seen)
    }
}

/// Panes whose agent wants you (blocked or unseen done), most urgent first, in tab order.
pub fn urgent(tabs: &[Vec<Seat>]) -> Vec<usize> {
    let mut urgent: Vec<(u8, usize)> =
        tabs.iter().flatten().filter_map(|s| s.look().filter(|l| l.urgency >= 2).map(|l| (l.urgency, s.id))).collect();
    urgent.sort_by_key(|&(u, _)| std::cmp::Reverse(u));
    urgent.into_iter().map(|(_, id)| id).collect()
}

/// Where "next agent" goes from pane `focus`: the urgent pane after it, cycling, or the
/// first one if it isn't urgent. None if no other pane is urgent.
pub fn next_urgent(tabs: &[Vec<Seat>], focus: usize) -> Option<usize> {
    let urgent = urgent(tabs);
    let next = match urgent.iter().position(|&id| id == focus) {
        Some(i) => urgent.get(i + 1).or(urgent.first()),
        None => urgent.first(),
    };
    next.copied().filter(|&id| id != focus)
}

/// The dot on a tab's label: the color of its most urgent agent, if any is not quiet.
pub fn tab_dot(tab: &[Seat]) -> Option<u32> {
    tab.iter().filter_map(Seat::look).filter(|l| l.urgency > 0).max_by_key(|l| l.urgency).map(|l| l.color)
}

/// The tab bar's summary, e.g. ["1 blocked", "2 working"].
pub fn summary<'a>(seats: impl IntoIterator<Item = Seat<'a>>) -> Vec<String> {
    let mut counts = [0usize; 4];
    for l in seats.into_iter().filter_map(|s| s.look()) {
        counts[l.urgency as usize] += 1;
    }
    [(3, "blocked"), (2, "done"), (1, "working")]
        .into_iter()
        .filter(|&(u, _)| counts[u] > 0)
        .map(|(u, what)| format!("{} {what}", counts[u]))
        .collect()
}

/// One agent in the sidebar.
#[derive(PartialEq, Debug)]
pub struct Row {
    pub id: usize,
    /// Index of its tab.
    pub tab: usize,
    pub dot: u32,
    pub head: String,
    pub sub: String,
}

/// The sidebar: every pane with an agent, in tab order.
pub fn rows(tabs: &[Vec<Seat>]) -> Vec<Row> {
    let mut rows = Vec::new();
    for (ti, tab) in tabs.iter().enumerate() {
        for s in tab {
            let (Some(a), Some(l)) = (s.agent, s.look()) else { continue };
            let title = if s.title.is_empty() { "shell" } else { s.title };
            let sub = match a.state {
                AgentState::Blocked if !a.message.is_empty() => a.message.clone(),
                AgentState::Blocked => "needs you".into(),
                _ => format!("tab {} · {title}", ti + 1),
            };
            let head = format!("{}  {}", a.name, a.state.name());
            rows.push(Row { id: s.id, tab: ti, dot: l.color, head, sub });
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(state: AgentState) -> Agent {
        Agent { name: "claude".into(), state, message: String::new(), reports_working: false }
    }

    fn seat(id: usize, agent: Option<&Agent>, seen: bool) -> Seat<'_> {
        Seat { id, agent, seen, title: "" }
    }

    #[test]
    fn looks_and_seen() {
        let [w, b, d, i] = [AgentState::Working, AgentState::Blocked, AgentState::Done, AgentState::Idle].map(agent);
        assert_eq!(look(None, false), None);
        assert_eq!(look(Some(&w), true), Some(Look { color: WORKING, border: true, urgency: 1 }));
        assert_eq!(look(Some(&d), false), Some(Look { color: DONE, border: true, urgency: 2 }));
        // A done agent stops wanting you once seen; a blocked one doesn't.
        assert_eq!(look(Some(&d), true), Some(Look { color: QUIET, border: false, urgency: 0 }));
        assert_eq!(look(Some(&b), true), Some(Look { color: BLOCKED, border: true, urgency: 3 }));
        assert_eq!(look(Some(&b), false).map(|l| l.urgency), Some(3));
        assert_eq!(look(Some(&i), false), Some(Look { color: QUIET, border: false, urgency: 0 }));
    }

    #[test]
    fn next_agent() {
        let [w, b, d, i] = [AgentState::Working, AgentState::Blocked, AgentState::Done, AgentState::Idle].map(agent);
        // Nothing urgent: working, idle, seen done, no agent.
        let calm = vec![vec![seat(0, Some(&w), false), seat(1, Some(&i), false)], vec![seat(2, Some(&d), true), seat(3, None, false)]];
        assert!(urgent(&calm).is_empty());
        assert_eq!(next_urgent(&calm, 0), None);

        // Only the focused pane is urgent: stay.
        let alone = vec![vec![seat(0, Some(&b), true), seat(1, Some(&w), false)]];
        assert_eq!(next_urgent(&alone, 0), None);
        assert_eq!(next_urgent(&alone, 1), Some(0));

        // Blocked before done, tab order within the same urgency.
        let tabs = vec![
            vec![seat(0, Some(&d), false), seat(1, Some(&w), false)],
            vec![seat(2, Some(&b), false), seat(3, Some(&d), false)],
            vec![seat(4, Some(&b), true), seat(5, Some(&d), true)],
        ];
        assert_eq!(urgent(&tabs), [2, 4, 0, 3]);
        // From a pane that isn't urgent: the most urgent.
        assert_eq!(next_urgent(&tabs, 1), Some(2));
        assert_eq!(next_urgent(&tabs, 5), Some(2));
        // Down the list, cycling past the end.
        assert_eq!(next_urgent(&tabs, 2), Some(4));
        assert_eq!(next_urgent(&tabs, 4), Some(0));
        assert_eq!(next_urgent(&tabs, 0), Some(3));
        assert_eq!(next_urgent(&tabs, 3), Some(2));
    }

    #[test]
    fn dots_and_summary() {
        let [w, b, d, i] = [AgentState::Working, AgentState::Blocked, AgentState::Done, AgentState::Idle].map(agent);
        assert_eq!(tab_dot(&[]), None);
        assert_eq!(tab_dot(&[seat(0, None, false), seat(1, Some(&i), false), seat(2, Some(&d), true)]), None);
        assert_eq!(tab_dot(&[seat(0, Some(&w), false), seat(1, Some(&d), false)]), Some(DONE));
        assert_eq!(tab_dot(&[seat(0, Some(&b), false), seat(1, Some(&w), false)]), Some(BLOCKED));
        assert_eq!(tab_dot(&[seat(0, Some(&d), true), seat(1, Some(&w), false)]), Some(WORKING));

        assert!(summary([seat(0, None, false), seat(1, Some(&i), false)]).is_empty());
        let all = [
            seat(0, Some(&w), false),
            seat(1, Some(&b), true),
            seat(2, Some(&w), false),
            seat(3, Some(&d), true),
            seat(4, Some(&d), false),
        ];
        assert_eq!(summary(all), ["1 blocked", "1 done", "2 working"]);
    }

    #[test]
    fn sidebar_rows() {
        let w = agent(AgentState::Working);
        let i = Agent { name: "codex".into(), ..agent(AgentState::Idle) };
        let b = agent(AgentState::Blocked);
        let asks = Agent { message: "Claude needs your permission".into(), ..agent(AgentState::Blocked) };
        let tabs = vec![
            vec![seat(0, None, false), Seat { title: "vim", ..seat(1, Some(&w), false) }],
            vec![seat(2, Some(&b), false), seat(3, Some(&i), false), seat(4, Some(&asks), true)],
        ];
        let row = |id, tab, dot, head: &str, sub: &str| Row { id, tab, dot, head: head.into(), sub: sub.into() };
        assert_eq!(
            rows(&tabs),
            [
                row(1, 0, WORKING, "claude  working", "tab 1 · vim"),
                row(2, 1, BLOCKED, "claude  blocked", "needs you"),
                row(3, 1, QUIET, "codex  idle", "tab 2 · shell"),
                row(4, 1, BLOCKED, "claude  blocked", "Claude needs your permission"),
            ]
        );
    }
}
