//! Where a sticky window belongs when the user switches workspace.
//!
//! niri has no sticky windows: nsticky emulates them by moving a tracked window
//! to the workspace the user focused. Which workspace that is depends on the
//! rule that matched the window:
//!
//! - no `output` pin: follow the focused workspace, wherever it is;
//! - `output = ["DP-2", "DP-1"]`: stay on `DP-2` while it is connected, fall
//!   back to `DP-1`, and keep following the focused workspace while neither is
//!   available. Two windows of the same application can therefore be pinned to
//!   different monitors, and the same window can be pinned to a list of
//!   monitors with different priorities.

use crate::niri::WorkspaceInfo;
use std::collections::HashSet;

/// The workspace layout reported by the compositor.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Layout {
    workspaces: Vec<WorkspaceInfo>,
}

impl Layout {
    pub fn new(workspaces: Vec<WorkspaceInfo>) -> Self {
        Self { workspaces }
    }

    /// Output a workspace lives on.
    pub fn output_of(&self, workspace_id: u64) -> Option<&str> {
        self.workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .and_then(|workspace| workspace.output.as_deref())
    }

    /// Whether an output is there. niri only reports workspaces of connected
    /// outputs, so their presence is what "connected" means here.
    pub fn is_connected(&self, output: &str) -> bool {
        self.workspaces
            .iter()
            .any(|workspace| workspace.output.as_deref() == Some(output))
    }

    /// Active workspace of every output that has one.
    pub fn active_workspace_of_outputs(&self) -> Vec<(String, u64)> {
        self.workspaces
            .iter()
            .filter(|workspace| workspace.is_active)
            .filter_map(|workspace| {
                workspace
                    .output
                    .clone()
                    .map(|output| (output, workspace.id))
            })
            .collect()
    }

    /// The empty workspace at the bottom of an output: niri's tail. Middle
    /// workspaces are the ones niri deletes on its own, so the tail is where a
    /// name can be parked without disturbing anything.
    ///
    /// Only an unnamed, empty, inactive workspace qualifies. A named one belongs
    /// to the user (niri keeps named workspaces on purpose) and a parking area
    /// is named while in use, so neither can be taken over by accident.
    pub fn bottom_empty_workspace(&self, output: &str, busy: &HashSet<u64>) -> Option<u64> {
        self.workspaces
            .iter()
            .filter(|workspace| workspace.output.as_deref() == Some(output))
            .filter(|workspace| !busy.contains(&workspace.id) && !workspace.is_active)
            .filter(|workspace| workspace.name.is_none())
            .max_by_key(|workspace| workspace.idx)
            .map(|workspace| workspace.id)
    }

    /// Workspace that is active on an output.
    pub fn active_workspace_of(&self, output: &str) -> Option<u64> {
        self.workspaces
            .iter()
            .find(|workspace| workspace.output.as_deref() == Some(output) && workspace.is_active)
            .map(|workspace| workspace.id)
    }
}

/// Moves needed so the parking workspaces sit at the end of their output, on
/// top of the empty tail niri always keeps there.
///
/// `names` is ordered top to bottom (the first one ends up above the second),
/// and only outputs that actually host one of them are touched. Each move is
/// `(workspace id, target position)`, 1-based like niri's IPC.
pub fn parking_order_moves(
    workspaces: &[WorkspaceInfo],
    occupied: &HashSet<u64>,
    names: &[&str],
) -> Vec<(u64, usize)> {
    let mut moves = Vec::new();

    let mut outputs: Vec<&str> = workspaces
        .iter()
        .filter_map(|workspace| workspace.output.as_deref())
        .collect();
    outputs.sort_unstable();
    outputs.dedup();

    for output in outputs {
        let mut ordered: Vec<&WorkspaceInfo> = workspaces
            .iter()
            .filter(|workspace| workspace.output.as_deref() == Some(output))
            .collect();
        ordered.sort_by_key(|workspace| workspace.idx);

        // Only the parking workspaces that exist, in the configured order.
        let present: Vec<&str> = names
            .iter()
            .copied()
            .filter(|name| {
                ordered
                    .iter()
                    .any(|workspace| workspace.name.as_deref() == Some(*name))
            })
            .collect();
        if present.is_empty() || is_parking_order(&ordered, occupied, &present) {
            continue;
        }

        // Move them top to bottom, each to the end: niri keeps a fresh empty
        // workspace below every one of them.
        let count = ordered.len();
        for (offset, name) in present.iter().enumerate() {
            if let Some(workspace) = ordered
                .iter()
                .find(|workspace| workspace.name.as_deref() == Some(*name))
            {
                moves.push((workspace.id, count + offset));
            }
        }
    }

    moves
}

/// Whether the parking workspaces are already the last ones of their output,
/// with only empty unnamed workspaces (niri's tail) below them.
fn is_parking_order(ordered: &[&WorkspaceInfo], occupied: &HashSet<u64>, names: &[&str]) -> bool {
    let mut matched = 0;

    for workspace in ordered.iter().rev() {
        if matched == names.len() {
            return true;
        }
        // An empty workspace without a name is the tail niri keeps around.
        if workspace.name.is_none() && !occupied.contains(&workspace.id) {
            continue;
        }
        // Bottom-most parking workspace first.
        if workspace.name.as_deref() != Some(names[names.len() - 1 - matched]) {
            return false;
        }
        matched += 1;
    }

    matched == names.len()
}

/// Workspace a sticky window should be moved to when `focused_workspace` became
/// focused, or `None` to leave the window where it is.
///
/// `pin` is the output preference list of the matching rule (empty for none),
/// and `current_output` is where the window is right now.
pub fn pin_target(
    pin: &[String],
    current_output: Option<&str>,
    layout: &Layout,
    focused_workspace: u64,
) -> Option<u64> {
    if pin.is_empty() {
        return Some(focused_workspace);
    }

    // First pinned output that is actually connected.
    let Some(target) = pin.iter().find(|output| layout.is_connected(output)) else {
        // Nothing pinned is connected: follow the focused workspace so the
        // window stays reachable until the output returns.
        return Some(focused_workspace);
    };

    if current_output == Some(target.as_str()) {
        // Already on its output: follow it only when that output is the one
        // that got the new focused workspace.
        return (layout.output_of(focused_workspace) == Some(target.as_str()))
            .then_some(focused_workspace);
    }

    // The window drifted (or its output was reconnected): put it back on its
    // output, on the workspace that is active there.
    layout.active_workspace_of(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// niri's tail: a workspace with no name.
    fn unnamed(id: u64, output: &str, active: bool) -> WorkspaceInfo {
        WorkspaceInfo {
            id,
            name: None,
            output: Some(output.to_string()),
            is_active: active,
            ..WorkspaceInfo::default_for_tests()
        }
    }

    fn workspace(id: u64, name: &str, output: &str, active: bool) -> WorkspaceInfo {
        WorkspaceInfo {
            id,
            name: Some(name.to_string()),
            output: Some(output.to_string()),
            is_active: active,
            ..WorkspaceInfo::default_for_tests()
        }
    }

    /// Two monitors: DP-1 with workspace 1 active, DP-2 with workspace 2 active.
    fn two_outputs() -> Layout {
        Layout::new(vec![
            workspace(1, "one", "DP-1", true),
            workspace(3, "three", "DP-1", false),
            workspace(2, "two", "DP-2", true),
        ])
    }

    fn pin(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn test_layout_lookup() {
        let layout = two_outputs();
        assert_eq!(layout.output_of(1), Some("DP-1"));
        assert_eq!(layout.output_of(3), Some("DP-1"));
        assert_eq!(layout.output_of(2), Some("DP-2"));
        assert_eq!(layout.output_of(99), None);
        assert!(layout.is_connected("DP-1"));
        assert!(!layout.is_connected("HDMI-A-1"));
        assert_eq!(layout.active_workspace_of("DP-2"), Some(2));
    }

    #[test]
    fn test_bottom_empty_workspace_never_takes_a_named_one() {
        let layout = Layout::new(vec![
            unnamed(1, "DP-1", false),
            workspace(2, "parking", "DP-1", false),
            workspace(3, "three", "DP-1", false),
        ]);
        let busy: HashSet<u64> = HashSet::new();

        assert_eq!(
            layout.bottom_empty_workspace("DP-1", &busy),
            Some(1),
            "the unnamed one, even when the named empties are lower"
        );

        // Nothing unnamed to take, and the active one is in use either way.
        let layout = Layout::new(vec![
            workspace(2, "parking", "DP-1", false),
            workspace(3, "three", "DP-1", false),
            unnamed(4, "DP-1", true),
        ]);
        assert_eq!(
            layout.bottom_empty_workspace("DP-1", &busy),
            None,
            "a named or active workspace is never taken over"
        );
    }

    #[test]
    fn test_bottom_empty_workspace_is_the_last_one_of_the_output() {
        let mut layout = two_outputs();
        for (idx, id) in [(1u8, 1u64), (2, 3)] {
            let mut workspaces = layout.workspaces.clone();
            workspaces
                .iter_mut()
                .find(|workspace| workspace.id == id)
                .unwrap()
                .idx = idx;
            layout = Layout::new(workspaces);
        }

        let layout = Layout::new(vec![
            unnamed(1, "DP-1", true),
            unnamed(2, "DP-1", false),
            unnamed(3, "DP-1", false),
            unnamed(4, "DP-2", false),
        ]);
        let busy: HashSet<u64> = HashSet::new();

        assert_eq!(
            layout.bottom_empty_workspace("DP-1", &busy),
            Some(3),
            "the highest index on DP-1"
        );
        assert_eq!(
            layout.bottom_empty_workspace("DP-2", &busy),
            Some(4),
            "the only one on DP-2"
        );
        assert_eq!(layout.bottom_empty_workspace("HDMI-A-1", &busy), None);

        // A busy bottom workspace is skipped in favour of the one above it.
        let busy: HashSet<u64> = [3].into();
        assert_eq!(layout.bottom_empty_workspace("DP-1", &busy), Some(2));
    }

    #[test]
    fn test_parking_order_is_accepted_when_it_is_already_right() {
        let layout = Layout::new(vec![
            workspace(1, "one", "DP-1", true),
            workspace(2, "scratchpad", "DP-1", false),
            workspace(3, "stage", "DP-1", false),
            workspace(4, "", "DP-1", false), // the empty tail
        ]);
        let occupied: HashSet<u64> = [1, 2, 3].into();
        let _ = layout;
        let ordered: Vec<&WorkspaceInfo> = Vec::new();
        let _ = ordered;

        let workspaces = vec![
            workspace(1, "one", "DP-1", true),
            workspace(2, "scratchpad", "DP-1", false),
            workspace(3, "stage", "DP-1", false),
            WorkspaceInfo {
                id: 4,
                idx: 4,
                name: None,
                output: Some("DP-1".to_string()),
                is_active: false,
            },
        ];
        assert!(
            parking_order_moves(&workspaces, &occupied, &["scratchpad", "stage"]).is_empty(),
            "already at the end"
        );
    }

    #[test]
    fn test_parking_order_moves_them_below_new_workspaces() {
        let occupied: HashSet<u64> = [1, 2, 3, 5].into();
        // A window was opened on the tail, so a new workspace (5) sits below.
        let workspaces = vec![
            workspace(1, "one", "DP-1", true),
            workspace(2, "scratchpad", "DP-1", false),
            workspace(3, "stage", "DP-1", false),
            workspace(5, "five", "DP-1", false),
            WorkspaceInfo {
                id: 6,
                idx: 5,
                name: None,
                output: Some("DP-1".to_string()),
                is_active: false,
            },
        ];

        let moves = parking_order_moves(&workspaces, &occupied, &["scratchpad", "stage"]);

        assert_eq!(
            moves,
            vec![(2, 5), (3, 6)],
            "scratchpad first, then stage, each to the end"
        );
    }

    #[test]
    fn test_parking_order_fixes_the_relative_order() {
        let occupied: HashSet<u64> = [1, 2, 3].into();
        // Stage ended up above the scratchpad.
        let workspaces = vec![
            workspace(1, "one", "DP-1", true),
            workspace(3, "stage", "DP-1", false),
            workspace(2, "scratchpad", "DP-1", false),
        ];

        let moves = parking_order_moves(&workspaces, &occupied, &["scratchpad", "stage"]);

        assert_eq!(moves, vec![(2, 3), (3, 4)]);
    }

    #[test]
    fn test_parking_order_only_touches_its_own_output() {
        let occupied: HashSet<u64> = [1, 2, 3].into();
        let workspaces = vec![
            workspace(1, "one", "DP-1", true),
            workspace(2, "scratchpad", "DP-1", false),
            workspace(3, "stage", "DP-2", false),
            workspace(4, "four", "DP-2", false),
        ];

        let moves = parking_order_moves(&workspaces, &occupied, &["scratchpad", "stage"]);

        assert_eq!(
            moves,
            vec![(3, 2)],
            "only DP-2 needs a move, and it goes to the end of that output"
        );
    }

    #[test]
    fn test_window_without_a_pin_follows_the_focused_workspace() {
        let layout = two_outputs();
        assert_eq!(pin_target(&[], Some("DP-2"), &layout, 7), Some(7));
    }

    #[test]
    fn test_pinned_window_ignores_activations_on_other_outputs() {
        let layout = two_outputs();
        // Window pinned to DP-2, focused workspace 9 lives on DP-1.
        let layout_with_foreign_focus = Layout::new(vec![
            workspace(1, "one", "DP-1", true),
            workspace(9, "nine", "DP-1", true),
            workspace(2, "two", "DP-2", true),
        ]);
        assert_eq!(
            pin_target(&pin(&["DP-2"]), Some("DP-2"), &layout_with_foreign_focus, 9),
            None,
            "a window pinned to DP-2 must not jump to DP-1"
        );

        // Its own output's activation does move it.
        let own = Layout::new(vec![
            workspace(1, "one", "DP-1", true),
            workspace(5, "five", "DP-2", true),
        ]);
        assert_eq!(pin_target(&pin(&["DP-2"]), Some("DP-2"), &own, 5), Some(5));
        assert!(layout.is_connected("DP-1"));
    }

    #[test]
    fn test_pinned_window_returns_to_its_output_when_it_drifted() {
        let layout = two_outputs();
        // The window is on DP-1 but belongs to DP-2: it goes back to the
        // workspace that is active there.
        assert_eq!(
            pin_target(&pin(&["DP-2"]), Some("DP-1"), &layout, 3),
            Some(2)
        );
    }

    #[test]
    fn test_output_preference_order_is_respected() {
        let layout = Layout::new(vec![
            workspace(1, "one", "DP-1", true),
            workspace(2, "two", "DP-2", true),
        ]);

        // DP-2 is listed first and is connected: DP-1 activations are ignored.
        assert_eq!(
            pin_target(&pin(&["DP-2", "DP-1"]), Some("DP-2"), &layout, 1),
            None
        );
        // Only DP-1 is connected: the window falls back to it.
        let only_first = Layout::new(vec![workspace(1, "one", "DP-1", true)]);
        assert_eq!(
            pin_target(&pin(&["DP-2", "DP-1"]), Some("DP-1"), &only_first, 1),
            Some(1)
        );
    }

    #[test]
    fn test_window_follows_focus_while_no_pinned_output_is_connected() {
        let layout = Layout::new(vec![workspace(1, "one", "DP-1", true)]);
        assert_eq!(
            pin_target(&pin(&["HDMI-A-1"]), Some("DP-1"), &layout, 1),
            Some(1),
            "an unplugged monitor must not make the window unreachable"
        );
    }

    #[test]
    fn test_window_on_a_unknown_workspace_falls_back_to_its_output() {
        let layout = two_outputs();
        // current_output is unknown (window had no workspace), pin applies.
        assert_eq!(
            pin_target(&pin(&["DP-1"]), None, &layout, 3),
            Some(1),
            "it goes to the workspace active on DP-1"
        );
    }

    #[test]
    fn test_parking_order_when_the_areas_are_the_whole_output() {
        // Nothing above or below them, not even niri's tail: the scan reaches
        // the end of the output and finds nothing out of place.
        let occupied: HashSet<u64> = [1, 2].into();
        let workspaces = vec![
            workspace(1, "scratchpad", "DP-1", false),
            workspace(2, "stage", "DP-1", false),
        ];

        assert!(parking_order_moves(&workspaces, &occupied, &["scratchpad", "stage"]).is_empty());
    }

    #[test]
    fn test_parking_order_skips_an_area_that_does_not_exist_yet() {
        // Only the scratchpad was ever used, so "stage" has no workspace to
        // move and must not hold the scratchpad in place.
        let at_the_bottom = vec![
            workspace(1, "one", "DP-1", true),
            workspace(2, "scratchpad", "DP-1", false),
        ];
        let occupied: HashSet<u64> = [1, 2].into();
        assert!(
            parking_order_moves(&at_the_bottom, &occupied, &["scratchpad", "stage"]).is_empty()
        );

        // With a window opened below it, it still goes to the end.
        let occupied: HashSet<u64> = [1, 2, 3].into();
        let below = vec![
            workspace(1, "one", "DP-1", true),
            workspace(2, "scratchpad", "DP-1", false),
            workspace(3, "three", "DP-1", false),
        ];
        assert_eq!(
            parking_order_moves(&below, &occupied, &["scratchpad", "stage"]),
            vec![(2, 3)],
            "the scratchpad moves, the missing stage contributes nothing"
        );
    }
}
