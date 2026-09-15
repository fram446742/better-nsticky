//! In-memory [`Niri`] for tests.
//!
//! Models the workspace strip per output (a move renumbers `idx`), moves that
//! update the window's own `workspace_id`, and a log of the trait calls
//! received. Any call can be made to fail (`fail_next`, `fail_nth`, `fail_move`,
//! `fail_queries`, `disconnect_next`), and in-flight calls are counted so a test
//! can assert that operations are serialized.

use parking_lot::Mutex;
use std::collections::HashSet;

use anyhow::{Result, anyhow, bail};

use super::{Niri, NiriFuture, Size, WindowInfo, WorkspaceInfo};

/// A trait call, so a test can pick which one to refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    Windows,
    Workspaces,
    ActiveWindow,
    ActiveWorkspace,
    MoveWindow,
    NameWorkspace,
    UnnameWorkspace,
    Spawn,
    FocusWindow,
    MoveFloatingWindow,
    FloatWindow,
    MoveWorkspaceToIndex,
    ResizeWindow,
}

impl Call {
    fn name(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Workspaces => "workspaces",
            Self::ActiveWindow => "active_window",
            Self::ActiveWorkspace => "active_workspace",
            Self::MoveWindow => "move_window",
            Self::NameWorkspace => "name_workspace",
            Self::UnnameWorkspace => "unname_workspace",
            Self::Spawn => "spawn",
            Self::FocusWindow => "focus_window",
            Self::MoveFloatingWindow => "move_floating_window",
            Self::FloatWindow => "float_window",
            Self::MoveWorkspaceToIndex => "move_workspace_to_index",
            Self::ResizeWindow => "resize_window",
        }
    }
}

/// A failure a test asked for: after `skip` more calls of `call`, the next one
/// is refused.
#[derive(Debug, Clone, Copy)]
struct Injection {
    call: Call,
    skip: usize,
    /// Also stop answering, the way a compositor that died mid-request does.
    disconnect: bool,
}

/// A [`Niri`] whose reactions are fully controlled by the test.
#[derive(Debug, Default)]
pub struct FakeNiri {
    windows: Mutex<Vec<WindowInfo>>,
    workspaces: Mutex<Vec<WorkspaceInfo>>,
    active_window: Mutex<u64>,
    active_workspace: Mutex<u64>,
    moves: Mutex<Vec<(u64, u64)>>,
    failing_moves: Mutex<HashSet<u64>>,
    queries_fail: Mutex<bool>,
    injections: Mutex<Vec<Injection>>,
    /// In-flight trait calls and the highest seen, for serialization assertions.
    in_flight: Mutex<usize>,
    max_in_flight: Mutex<usize>,
    /// Workspace name/unname calls, in order, for assertions.
    named_actions: Mutex<Vec<String>>,
    /// Spawns, focuses and window styling, in order.
    window_actions: Mutex<Vec<String>>,
}

impl FakeNiri {
    pub fn new() -> Self {
        Self {
            active_window: Mutex::new(1),
            active_workspace: Mutex::new(10),
            ..Self::default()
        }
    }

    /// Fake with the given windows: `(id, app_id, title)`.
    pub fn with_windows(windows: &[(u64, &str, &str)]) -> Self {
        let fake = Self::new();
        *fake.windows.lock() = windows
            .iter()
            .map(|(id, app_id, title)| WindowInfo {
                id: *id,
                app_id: Some((*app_id).to_string()),
                title: Some((*title).to_string()),
                workspace_id: None,
                floating: false,
                size: None,
                position: None,
            })
            .collect();
        fake
    }

    /// Workspaces as niri reports them, `(id, name, output)` in strip order.
    pub fn set_workspaces(&self, workspaces: &[(u64, &str, &str)]) {
        let active = *self.active_workspace.lock();
        let mut per_output: std::collections::HashMap<String, u8> =
            std::collections::HashMap::new();

        *self.workspaces.lock() = workspaces
            .iter()
            .map(|(id, name, output)| {
                let idx = per_output.entry((*output).to_string()).or_insert(0);
                *idx += 1;
                WorkspaceInfo {
                    id: *id,
                    idx: *idx,
                    name: (!name.is_empty()).then(|| (*name).to_string()),
                    output: Some((*output).to_string()),
                    is_active: *id == active,
                }
            })
            .collect();
    }

    /// Put a window on a workspace, as niri would report it.
    pub fn set_window_workspace(&self, window_id: u64, workspace_id: u64) {
        for window in self.windows.lock().iter_mut() {
            if window.id == window_id {
                window.workspace_id = Some(workspace_id);
            }
        }
    }

    pub fn set_active_window(&self, id: u64) {
        *self.active_window.lock() = id;
    }

    pub fn set_active_workspace(&self, id: u64) {
        *self.active_workspace.lock() = id;
    }

    pub fn set_windows(&self, windows: &[(u64, &str, &str)]) {
        *self.windows.lock() = windows
            .iter()
            .map(|(id, app_id, title)| WindowInfo {
                id: *id,
                app_id: Some((*app_id).to_string()),
                title: Some((*title).to_string()),
                workspace_id: None,
                floating: false,
                size: None,
                position: None,
            })
            .collect();
    }

    /// Workspaces niri knows by name, for assertions.
    pub fn named_workspaces(&self) -> Vec<(u64, String)> {
        self.workspaces
            .lock()
            .iter()
            .filter_map(|workspace| workspace.name.clone().map(|name| (workspace.id, name)))
            .collect()
    }

    /// Mark a workspace as (in)active on its output. niri keeps one active
    /// workspace per output, so tests set several.
    pub fn set_workspace_active(&self, workspace_id: u64, active: bool) {
        for workspace in self.workspaces.lock().iter_mut() {
            if workspace.id == workspace_id {
                workspace.is_active = active;
            }
        }
    }

    /// Mark a window as floating, as niri would report it.
    pub fn set_window_floating(&self, window_id: u64, floating: bool) {
        for window in self.windows.lock().iter_mut() {
            if window.id == window_id {
                window.floating = floating;
            }
        }
    }

    /// Geometry a floating window would report.
    pub fn set_window_geometry(&self, window_id: u64, size: (i32, i32), position: (f64, f64)) {
        for window in self.windows.lock().iter_mut() {
            if window.id == window_id {
                window.size = Some(size);
                window.position = Some(position);
            }
        }
    }

    /// Make every move of this window fail.
    pub fn fail_move(&self, window_id: u64) {
        self.failing_moves.lock().insert(window_id);
    }

    /// Make every query fail, as if the compositor were unreachable.
    pub fn fail_queries(&self, fail: bool) {
        *self.queries_fail.lock() = fail;
    }

    /// Refuse the next call of this kind; later calls work again.
    pub fn fail_next(&self, call: Call) {
        self.fail_nth(call, 1);
    }

    /// Refuse the `nth` call of this kind from now on; the ones before it go
    /// through, so a test can fail a later step of a multi-call operation.
    pub fn fail_nth(&self, call: Call, nth: usize) {
        self.injections.lock().push(Injection {
            call,
            skip: nth.saturating_sub(1),
            disconnect: false,
        });
    }

    /// Refuse the next call of this kind and stop answering everything after
    /// it, the way a compositor that went away mid-request looks to a caller.
    pub fn disconnect_next(&self, call: Call) {
        self.injections.lock().push(Injection {
            call,
            skip: 0,
            disconnect: true,
        });
    }

    /// Refuse a call a test asked to fail, before it has any effect.
    fn check_failure(&self, call: Call) -> Result<()> {
        let mut injections = self.injections.lock();
        let Some(position) = injections
            .iter()
            .position(|injection| injection.call == call)
        else {
            return Ok(());
        };
        if injections[position].skip > 0 {
            injections[position].skip -= 1;
            return Ok(());
        }

        let injection = injections.remove(position);
        drop(injections);
        if injection.disconnect {
            *self.queries_fail.lock() = true;
            bail!("fake niri: connection closed during {}", call.name());
        }
        bail!("fake niri: injected failure for {}", call.name());
    }

    /// Windows in move order, with the workspace each went to.
    pub fn moves(&self) -> Vec<(u64, u64)> {
        self.moves.lock().clone()
    }

    /// Ids moved to this workspace.
    pub fn moved_to_id(&self, workspace_id: u64) -> Vec<u64> {
        self.moves
            .lock()
            .iter()
            .filter(|(_, destination)| *destination == workspace_id)
            .map(|(id, _)| *id)
            .collect()
    }

    fn name_workspace_inner(&self, name: &str, workspace_id: u64) -> Result<()> {
        self.check_queries()?;
        self.check_failure(Call::NameWorkspace)?;

        {
            let mut workspaces = self.workspaces.lock();
            let workspace = workspaces
                .iter_mut()
                .find(|candidate| candidate.id == workspace_id)
                .ok_or_else(|| anyhow!("fake niri: no workspace with id {workspace_id}"))?;
            workspace.name = Some(name.to_string());
        }

        self.named_actions
            .lock()
            .push(format!("name {workspace_id} {name}"));
        Ok(())
    }

    fn unname_workspace_inner(&self, workspace_id: u64) -> Result<()> {
        self.check_queries()?;
        self.check_failure(Call::UnnameWorkspace)?;

        {
            let mut workspaces = self.workspaces.lock();
            if let Some(workspace) = workspaces.iter_mut().find(|ws| ws.id == workspace_id) {
                workspace.name = None;
            }
        }

        self.named_actions
            .lock()
            .push(format!("unname {workspace_id}"));
        Ok(())
    }

    fn move_window_inner(&self, window_id: u64, workspace_id: u64) -> Result<()> {
        self.check_queries()?;
        self.check_failure(Call::MoveWindow)?;
        if self.failing_moves.lock().contains(&window_id) {
            bail!("fake niri refused to move window {window_id}");
        }
        self.moves.lock().push((window_id, workspace_id));

        // A real move shows up in the window list too.
        for window in self.windows.lock().iter_mut() {
            if window.id == window_id {
                window.workspace_id = Some(workspace_id);
            }
        }

        Ok(())
    }

    /// Spawns, focus and styling calls, in order.
    pub fn window_actions(&self) -> Vec<String> {
        self.window_actions.lock().clone()
    }

    /// Workspace naming calls, in order.
    pub fn workspace_naming(&self) -> Vec<String> {
        self.named_actions.lock().clone()
    }

    /// Highest number of trait calls that were in flight at the same time.
    pub fn max_in_flight_calls(&self) -> usize {
        *self.max_in_flight.lock()
    }

    /// Enter a trait call, yielding so concurrent calls can interleave.
    async fn enter(&self) {
        {
            let mut in_flight = self.in_flight.lock();
            *in_flight += 1;
            let mut max = self.max_in_flight.lock();
            *max = (*max).max(*in_flight);
        }
        tokio::task::yield_now().await;
    }

    fn exit(&self) {
        *self.in_flight.lock() -= 1;
    }

    fn check_queries(&self) -> Result<()> {
        if *self.queries_fail.lock() {
            bail!("fake niri is unreachable");
        }
        Ok(())
    }
}

impl Niri for FakeNiri {
    fn windows(&self) -> NiriFuture<'_, Vec<WindowInfo>> {
        Box::pin(async move {
            self.enter().await;
            let result = self
                .check_queries()
                .and_then(|()| self.check_failure(Call::Windows))
                .map(|()| self.windows.lock().clone());
            self.exit();
            result
        })
    }

    fn workspaces(&self) -> NiriFuture<'_, Vec<WorkspaceInfo>> {
        Box::pin(async move {
            self.enter().await;
            let result = self
                .check_queries()
                .and_then(|()| self.check_failure(Call::Workspaces))
                .map(|()| self.workspaces.lock().clone());
            self.exit();
            result
        })
    }

    fn active_window(&self) -> NiriFuture<'_, u64> {
        Box::pin(async move {
            self.enter().await;
            let result = self
                .check_queries()
                .and_then(|()| self.check_failure(Call::ActiveWindow))
                .map(|()| *self.active_window.lock());
            self.exit();
            result
        })
    }

    fn active_workspace(&self) -> NiriFuture<'_, u64> {
        Box::pin(async move {
            self.enter().await;
            let result = self
                .check_queries()
                .and_then(|()| self.check_failure(Call::ActiveWorkspace))
                .map(|()| *self.active_workspace.lock());
            self.exit();
            result
        })
    }

    fn move_window<'a>(&'a self, window_id: u64, workspace_id: u64) -> NiriFuture<'a, ()> {
        Box::pin(async move {
            self.enter().await;
            let result = self.move_window_inner(window_id, workspace_id);
            self.exit();
            result
        })
    }

    fn name_workspace<'a>(&'a self, name: &'a str, workspace_id: u64) -> NiriFuture<'a, ()> {
        Box::pin(async move {
            self.enter().await;
            let result = self.name_workspace_inner(name, workspace_id);
            self.exit();
            result
        })
    }

    fn unname_workspace<'a>(&'a self, workspace_id: u64) -> NiriFuture<'a, ()> {
        Box::pin(async move {
            self.enter().await;
            let result = self.unname_workspace_inner(workspace_id);
            self.exit();
            result
        })
    }

    fn spawn<'a>(&'a self, command: &'a [String]) -> NiriFuture<'a, ()> {
        Box::pin(async move {
            self.check_queries()?;
            self.check_failure(Call::Spawn)?;
            self.window_actions
                .lock()
                .push(format!("spawn {}", command.join(" ")));
            Ok(())
        })
    }

    fn focus_window(&self, window_id: u64) -> NiriFuture<'_, ()> {
        Box::pin(async move {
            self.check_queries()?;
            self.check_failure(Call::FocusWindow)?;
            self.window_actions
                .lock()
                .push(format!("focus {window_id}"));
            Ok(())
        })
    }

    fn move_floating_window(&self, window_id: u64, x: f64, y: f64) -> NiriFuture<'_, ()> {
        Box::pin(async move {
            self.check_queries()?;
            self.check_failure(Call::MoveFloatingWindow)?;
            self.window_actions
                .lock()
                .push(format!("move {window_id} {x} {y}"));
            Ok(())
        })
    }

    fn float_window(&self, window_id: u64) -> NiriFuture<'_, ()> {
        Box::pin(async move {
            self.check_queries()?;
            self.check_failure(Call::FloatWindow)?;
            self.window_actions
                .lock()
                .push(format!("float {window_id}"));
            Ok(())
        })
    }

    fn move_workspace_to_index(&self, workspace_id: u64, index: usize) -> NiriFuture<'_, ()> {
        Box::pin(async move {
            self.check_queries()?;
            self.check_failure(Call::MoveWorkspaceToIndex)?;

            // Like niri: take the workspace out, put it back at the requested
            // position, and renumber the rest.
            let mut workspaces = self.workspaces.lock();
            let Some(position) = workspaces.iter().position(|ws| ws.id == workspace_id) else {
                bail!("fake niri: no workspace with id {workspace_id}");
            };
            let moved = workspaces.remove(position);
            let target = index.saturating_sub(1).min(workspaces.len());
            workspaces.insert(target, moved);
            for (position, workspace) in workspaces.iter_mut().enumerate() {
                workspace.idx = (position + 1) as u8;
            }
            drop(workspaces);

            self.named_actions
                .lock()
                .push(format!("movews {workspace_id} {index}"));
            Ok(())
        })
    }

    fn resize_window(
        &self,
        window_id: u64,
        width: Option<Size>,
        height: Option<Size>,
    ) -> NiriFuture<'_, ()> {
        Box::pin(async move {
            self.check_queries()?;
            self.check_failure(Call::ResizeWindow)?;
            let render = |size: Option<Size>, axis: &str| match size {
                Some(Size::Fixed(pixels)) => format!("{axis}={pixels}px"),
                Some(Size::Percent(percent)) => format!("{axis}={percent}%"),
                None => String::new(),
            };
            let parts: Vec<String> = [render(width, "width"), render(height, "height")]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect();
            if !parts.is_empty() {
                self.window_actions
                    .lock()
                    .push(format!("resize {window_id} {}", parts.join(" ")));
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake() -> FakeNiri {
        let fake = FakeNiri::with_windows(&[(1, "foot", "Terminal")]);
        fake.set_workspaces(&[(1, "one", "DP-1")]);
        fake
    }

    #[tokio::test]
    async fn an_injected_failure_refuses_one_call_and_nothing_else() {
        let niri = fake();
        niri.fail_next(Call::Windows);

        let error = niri.windows().await.unwrap_err();
        assert!(
            error.to_string().contains("injected failure for windows"),
            "{error}"
        );
        assert_eq!(niri.windows().await.unwrap().len(), 1);
        assert_eq!(niri.workspaces().await.unwrap().len(), 1, "other calls");
    }

    #[tokio::test]
    async fn a_refused_action_leaves_no_trace() {
        let niri = fake();
        niri.fail_next(Call::MoveWindow);

        assert!(niri.move_window(1, 1).await.is_err());
        assert!(niri.moves().is_empty(), "a refused move moves nothing");

        niri.move_window(1, 1).await.unwrap();
        assert_eq!(niri.moves(), vec![(1, 1)]);
        assert_eq!(
            niri.windows().await.unwrap()[0].workspace_id,
            Some(1),
            "the move that went through is reported"
        );

        niri.fail_next(Call::NameWorkspace);
        assert!(niri.name_workspace("stage", 1).await.is_err());
        assert_eq!(
            niri.named_workspaces(),
            vec![(1, "one".to_string())],
            "a refused name renames nothing"
        );
    }

    #[tokio::test]
    async fn fail_nth_lets_the_calls_before_it_through() {
        let niri = fake();
        niri.fail_nth(Call::Workspaces, 2);

        niri.workspaces().await.unwrap();
        assert!(
            niri.workspaces().await.is_err(),
            "the second call is refused"
        );
        niri.workspaces().await.unwrap();
    }

    #[tokio::test]
    async fn a_dropped_connection_fails_the_call_and_everything_after_it() {
        let niri = fake();
        niri.disconnect_next(Call::MoveWindow);

        let error = niri.move_window(1, 1).await.unwrap_err();
        assert!(error.to_string().contains("connection closed"), "{error}");

        assert!(niri.moves().is_empty(), "the window did not move");
        assert!(
            niri.workspaces().await.is_err(),
            "the compositor stays away afterwards"
        );
    }

    #[tokio::test]
    async fn a_move_of_a_workspace_that_is_not_there_is_refused() {
        let niri = fake();

        let error = niri.move_workspace_to_index(99, 1).await.unwrap_err();

        assert!(
            error.to_string().contains("no workspace with id 99"),
            "{error}"
        );
        assert!(niri.workspace_naming().is_empty());
    }

    #[tokio::test]
    async fn a_resize_of_one_axis_only_is_reported_for_that_axis() {
        let niri = fake();

        niri.resize_window(1, None, Some(Size::Fixed(400)))
            .await
            .unwrap();
        niri.resize_window(1, Some(Size::Percent(60.0)), None)
            .await
            .unwrap();
        niri.resize_window(1, None, None).await.unwrap();

        assert_eq!(
            niri.window_actions(),
            vec!["resize 1 height=400px", "resize 1 width=60%"],
            "an axis without a size is left alone"
        );
    }

    #[tokio::test]
    async fn a_failing_window_keeps_its_injection_apart_from_the_others() {
        let niri = fake();
        niri.fail_move(1);

        assert!(niri.move_window(1, 1).await.is_err(), "only this window");
        assert_eq!(niri.moves(), Vec::new());
    }
}
