//! Sticky/staged window bookkeeping and request orchestration.
//!
//! [`BusinessLogic`] owns the daemon's in-memory state: which windows are
//! sticky, which are parked on the stage workspace. Every move goes through
//! [`Niri`], and state is committed only once the compositor accepted it, so a
//! failed move leaves nothing to undo.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use crate::config::{Config, Scratchpad, StickyFollow, WindowAction, WindowFacts};
use crate::niri::{Niri, Size, WindowInfo};
use crate::pinning::{Layout, parking_order_moves, pin_target};
use crate::protocol::{Request, Response};
use crate::state_store::{StateStorage, StoredScratchpad, StoredStaged, StoredState, StoredSticky};

/// A window parked on the stage workspace.
#[derive(Debug, Clone, PartialEq)]
struct StagedWindow {
    /// Whether the window was sticky before it was staged; restoring puts it back.
    was_sticky: bool,
    /// Output pin the window had as a sticky window, restored with it.
    pin: Vec<String>,
    /// Geometry the window had when it was parked, so a scratchpad brings it back.
    size: Option<(i32, i32)>,
    position: Option<(f64, f64)>,
}

/// Outcome of an operation that touches several windows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchOutcome {
    /// Windows the operation was applied to.
    pub succeeded: Vec<u64>,
    /// Windows the compositor refused to move.
    pub failed: Vec<u64>,
}

impl BatchOutcome {
    /// Windows the operation succeeded on; refused ones are in `failed`.
    pub fn count(&self) -> usize {
        self.succeeded.len()
    }

    /// Message for the CLI: `Staged 2 windows, 1 failed: 5`.
    pub fn message(&self, verb: &str, noun: &str) -> String {
        let mut message = format!("{verb} {} {noun}", self.count());
        if !self.failed.is_empty() {
            let ids: Vec<String> = self.failed.iter().map(u64::to_string).collect();
            message.push_str(&format!(
                ", {} failed: {}",
                self.failed.len(),
                ids.join(", ")
            ));
        }
        message
    }
}

/// A window kept on every workspace, with the outputs it is pinned to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct StickyWindow {
    /// Output preference list from the matching rule; empty means follow focus.
    pin: Vec<String>,
}

/// A window parked by a scratchpad: separate from the stage, so a dropdown
/// terminal never shows up in `stage restore`.
#[derive(Debug, Clone, PartialEq)]
struct ScratchpadWindow {
    id: u64,
    was_sticky: bool,
    pin: Vec<String>,
    size: Option<(i32, i32)>,
    position: Option<(f64, f64)>,
}

#[derive(Default)]
struct AppState {
    sticky_windows: HashMap<u64, StickyWindow>,
    staged_windows: HashMap<u64, StagedWindow>,
    /// Window each scratchpad parked, so the same key shows it again.
    scratchpad_slots: std::collections::BTreeMap<String, ScratchpadWindow>,
    /// Windows a `[stage.*]` rule already parked in this daemon run, so an
    /// explicit restore is not undone by the next event the window emits.
    auto_staged: HashSet<u64>,
}

#[derive(Clone)]
pub struct BusinessLogic {
    state: Arc<Mutex<AppState>>,
    /// Swappable so `reload` applies a new configuration without a restart.
    config: Arc<RwLock<Arc<Config>>>,
    config_path: Arc<PathBuf>,
    niri: Arc<dyn Niri>,
    storage: Arc<dyn StateStorage>,
    /// Last state handed to the storage, so unchanged state is not rewritten
    /// (window events fire on every title change).
    persisted: Arc<Mutex<Option<StoredState>>>,
    /// Held for the duration of a command or an event: an operation reads the
    /// compositor, changes state and moves windows, and two of them at once
    /// would interleave those steps.
    operations: Arc<Mutex<()>>,
}

impl AppState {
    /// The state as it is persisted.
    fn snapshot(&self) -> StoredState {
        let mut sticky: Vec<StoredSticky> = self
            .sticky_windows
            .iter()
            .map(|(id, window)| StoredSticky::new(*id, window.pin.clone()))
            .collect();
        sticky.sort_unstable_by_key(StoredSticky::id);
        let mut staged: Vec<StoredStaged> = self
            .staged_windows
            .iter()
            .map(|(id, staged)| StoredStaged {
                id: *id,
                was_sticky: staged.was_sticky,
                outputs: staged.pin.clone(),
                size: staged.size,
                position: staged.position,
            })
            .collect();
        staged.sort_unstable_by_key(|staged| staged.id);

        let mut state = StoredState::new(sticky, staged);
        state.scratchpads = self
            .scratchpad_slots
            .iter()
            .map(|(label, window)| {
                (
                    label.clone(),
                    StoredScratchpad::Full {
                        id: window.id,
                        was_sticky: window.was_sticky,
                        outputs: window.pin.clone(),
                        size: window.size,
                        position: window.position,
                    },
                )
            })
            .collect();
        state
    }

    /// Restore a persisted state without validating it against the compositor.
    fn restore(&mut self, stored: StoredState) {
        self.sticky_windows = stored
            .sticky
            .into_iter()
            .map(StoredSticky::into_parts)
            .map(|(id, pin)| (id, StickyWindow { pin }))
            .collect();
        self.scratchpad_slots = stored
            .scratchpads
            .into_iter()
            .map(|(label, window)| {
                let record = window.into_record();
                (
                    label,
                    ScratchpadWindow {
                        id: record.id,
                        was_sticky: record.was_sticky,
                        pin: record.outputs,
                        size: record.size,
                        position: record.position,
                    },
                )
            })
            .collect();
        self.staged_windows = stored
            .staged
            .into_iter()
            .map(|staged| {
                (
                    staged.id,
                    StagedWindow {
                        was_sticky: staged.was_sticky,
                        pin: staged.outputs,
                        size: staged.size,
                        position: staged.position,
                    },
                )
            })
            .collect();
    }
}

/// A fallible operation as a protocol response, rendering the value with `message`.
fn reply<T>(result: Result<T>, message: impl FnOnce(T) -> String) -> Response {
    match result {
        Ok(value) => Response::success(message(value)),
        Err(e) => Response::error(e),
    }
}

/// A fallible operation as a response whose payload is its JSON encoding.
fn reply_json<T: serde::Serialize>(result: Result<T>) -> Response {
    match result {
        Ok(value) => match serde_json::to_string(&value) {
            Ok(data) => Response::data(data),
            Err(e) => Response::error(anyhow!("Failed to serialize response: {e}")),
        },
        Err(e) => Response::error(e),
    }
}

/// Sizes are the same shape on both sides, only the types differ.
fn to_niri_size(size: crate::config::Size) -> Size {
    match size {
        crate::config::Size::Fixed(pixels) => Size::Fixed(pixels),
        crate::config::Size::Percent(percent) => Size::Percent(percent),
    }
}

/// The state of a daemon that has never tracked a window.
fn empty_state() -> StoredState {
    StoredState::new(Vec::new(), Vec::new())
}

/// Ids of windows whose app id is exactly `app_id`.
fn ids_by_appid(windows: &[WindowInfo], app_id: &str) -> Vec<u64> {
    windows
        .iter()
        .filter(|window| window.app_id.as_deref() == Some(app_id))
        .map(|window| window.id)
        .collect()
}

/// Ids of windows whose title contains `title`.
fn ids_by_title(windows: &[WindowInfo], title: &str) -> Vec<u64> {
    windows
        .iter()
        .filter(|window| {
            window
                .title
                .as_deref()
                .is_some_and(|window_title| window_title.contains(title))
        })
        .map(|window| window.id)
        .collect()
}

impl BusinessLogic {
    pub fn new(
        config: Config,
        config_path: PathBuf,
        niri: Arc<dyn Niri>,
        storage: Arc<dyn StateStorage>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(AppState::default())),
            config: Arc::new(RwLock::new(Arc::new(config))),
            config_path: Arc::new(config_path),
            niri,
            storage,
            // Until `reconcile` reads the file, assume nothing is stored, so a
            // read-only request cannot overwrite the file with an empty snapshot.
            persisted: Arc::new(Mutex::new(Some(empty_state()))),
            operations: Arc::new(Mutex::new(())),
        }
    }

    /// Persist the current state, skipping writes when nothing changed. A
    /// failure to persist is logged, never fatal: window management keeps
    /// working on a read-only or full filesystem.
    async fn persist(&self) {
        let snapshot = { self.state.lock().await.snapshot() };
        {
            let mut persisted = self.persisted.lock().await;
            if persisted.as_ref() == Some(&snapshot) {
                return;
            }
            *persisted = Some(snapshot.clone());
        }

        if let Err(e) = self.storage.save(&snapshot) {
            tracing::warn!("Failed to persist state: {e:#}");
            // Let a later change try again.
            *self.persisted.lock().await = None;
        }
    }

    /// Ids of the windows currently sitting on the stage workspace.
    async fn windows_on_stage_workspace(
        &self,
        windows: &[WindowInfo],
    ) -> Result<Option<HashSet<u64>>> {
        let name = self.stage_workspace_name().await;
        let stage_workspace_id = self
            .niri
            .workspaces()
            .await?
            .into_iter()
            .find(|workspace| workspace.name.as_deref() == Some(name.as_str()))
            .map(|workspace| workspace.id);

        let Some(stage_workspace_id) = stage_workspace_id else {
            return Ok(None);
        };
        // Without workspace ids on the windows there is nothing to match on.
        if !windows.iter().any(|window| window.workspace_id.is_some()) {
            return Ok(None);
        }

        Ok(Some(
            windows
                .iter()
                .filter(|window| window.workspace_id == Some(stage_workspace_id))
                .map(|window| window.id)
                .collect(),
        ))
    }

    /// Rebuild the in-memory state after a (re)start.
    ///
    /// The stored state is authoritative for sticky windows, the compositor for
    /// what is staged: whatever sits on the stage workspace is adopted, so a
    /// stale state file cannot orphan the windows parked there.
    pub async fn reconcile(&self) -> Result<()> {
        let _operation = self.operations.lock().await;
        let stored = match self.storage.load() {
            Ok(stored) => stored,
            Err(e) => {
                tracing::warn!("Ignoring stored state: {e:#}");
                None
            }
        };

        // From here on "unchanged" means "unchanged since what is on disk":
        // the reconciled state is written whenever it differs from the file.
        *self.persisted.lock().await = Some(stored.clone().unwrap_or_else(empty_state));

        let windows = match self.windows().await {
            Ok(windows) => windows,
            Err(e) => {
                // Without the compositor the stored ids cannot be validated, so
                // keep them instead of dropping everything.
                tracing::warn!("Cannot reconcile with Niri ({e:#}); restoring stored state");
                if let Some(stored) = stored {
                    self.state.lock().await.restore(stored);
                }
                return Ok(());
            }
        };

        let on_stage = match self.windows_on_stage_workspace(&windows).await {
            Ok(on_stage) => on_stage,
            Err(e) => {
                tracing::warn!("Failed to list workspaces ({e:#}); using stored state");
                None
            }
        };

        let exists = |id: u64| windows.iter().any(|window| window.id == id);
        let mut sticky: HashMap<u64, StickyWindow> = HashMap::new();
        let mut staged: HashMap<u64, StagedWindow> = HashMap::new();

        // Stored state supplies the sticky list, its output pins and the "was
        // sticky before being staged" memory, which the compositor cannot know.
        let (stored_sticky, stored_entries, stored_pads) = match stored {
            Some(stored) => (stored.sticky, stored.staged, stored.scratchpads),
            None => (Vec::new(), Vec::new(), std::collections::BTreeMap::new()),
        };

        sticky.extend(
            stored_sticky
                .into_iter()
                .map(StoredSticky::into_parts)
                .filter(|(id, _)| exists(*id))
                .map(|(id, pin)| (id, StickyWindow { pin })),
        );
        let mut stored_staged: HashMap<u64, StoredStaged> = HashMap::new();
        stored_staged.extend(
            stored_entries
                .into_iter()
                .filter(|entry| exists(entry.id))
                .map(|entry| (entry.id, entry)),
        );

        // Scratchpads keep the label of the slot they belong to, which only the
        // file knows; a window that is gone takes its slot with it.
        let scratchpads: std::collections::BTreeMap<String, ScratchpadWindow> = stored_pads
            .into_iter()
            .filter(|(_, window)| exists(window.id()))
            .map(|(label, window)| {
                let record = window.into_record();
                (
                    label,
                    ScratchpadWindow {
                        id: record.id,
                        was_sticky: record.was_sticky,
                        pin: record.outputs,
                        size: record.size,
                        position: record.position,
                    },
                )
            })
            .collect();

        match &on_stage {
            // A window that left the stage workspace is not staged any more,
            // even when the stored state still lists it.
            Some(on_stage) => {
                for id in on_stage {
                    let stored = stored_staged.get(id);
                    staged.insert(
                        *id,
                        StagedWindow {
                            was_sticky: stored.is_some_and(|entry| entry.was_sticky),
                            pin: stored
                                .map(|entry| entry.outputs.clone())
                                .unwrap_or_default(),
                            size: stored.and_then(|entry| entry.size),
                            position: stored.and_then(|entry| entry.position),
                        },
                    );
                }
            }
            // Without workspace information, trust the stored staged windows.
            None => {
                for (id, entry) in stored_staged {
                    staged.insert(
                        id,
                        StagedWindow {
                            was_sticky: entry.was_sticky,
                            pin: entry.outputs,
                            size: entry.size,
                            position: entry.position,
                        },
                    );
                }
            }
        }

        // A window is either sticky, staged or parked by a scratchpad.
        let staged_ids: HashSet<u64> = staged.keys().copied().collect();
        let pad_ids: HashSet<u64> = scratchpads.values().map(|window| window.id).collect();
        sticky.retain(|id, _| !staged_ids.contains(id) && !pad_ids.contains(id));
        staged.retain(|id, _| !pad_ids.contains(id));

        let (sticky_count, staged_count, pad_count) = {
            let mut state = self.state.lock().await;
            state.sticky_windows = sticky;
            state.staged_windows = staged;
            state.scratchpad_slots = scratchpads;
            (
                state.sticky_windows.len(),
                state.staged_windows.len(),
                state.scratchpad_slots.len(),
            )
        };

        tracing::info!(
            "Reconciled state: {sticky_count} sticky, {staged_count} staged, {pad_count} parked by a scratchpad"
        );
        self.persist().await;
        Ok(())
    }

    /// The configuration in use.
    async fn config(&self) -> Arc<Config> {
        self.config.read().await.clone()
    }

    /// Re-read the configuration file and apply it.
    ///
    /// The running configuration is kept when the file cannot be read or
    /// parsed, so a typo never leaves the daemon without rules.
    pub async fn reload_from_disk(&self) -> Result<String> {
        let _operation = self.operations.lock().await;

        let config = Config::load(self.config_path.as_path()).with_context(|| {
            format!(
                "Keeping the running configuration: {}",
                self.config_path.display()
            )
        })?;

        self.apply_config(config).await
    }

    /// Replace the configuration and report what changed.
    async fn apply_config(&self, config: Config) -> Result<String> {
        let rules = config.rules().len();
        *self.config.write().await = Arc::new(config);
        tracing::info!("Configuration reloaded: {rules} rule(s)");

        // Tracked windows keep their state but take their new pins, so editing
        // `output` only needs a reload.
        let updated = self.reevaluate_pins().await.unwrap_or_default();

        Ok(format!(
            "Reloaded configuration: {rules} rule(s), {updated} window(s) re-pinned"
        ))
    }

    /// Re-apply the rules to the windows already tracked, returning how many
    /// pins changed. Windows nobody matches any more keep what they have.
    async fn reevaluate_pins(&self) -> Result<usize> {
        let windows = self.windows().await?;
        let layout = self.layout().await?;
        let config = self.config().await;

        // Decide the new pins first: `default_pin_for` reads the layout and the
        // configuration, and the state must not be held while doing that.
        let mut new_pins: Vec<(u64, Vec<String>)> = Vec::new();
        {
            let state = self.state.lock().await;
            for (id, window) in state.sticky_windows.iter() {
                let Some(current) = windows.iter().find(|candidate| candidate.id == *id) else {
                    continue;
                };
                let facts = WindowFacts {
                    app_id: current.app_id.as_deref(),
                    title: current.title.as_deref(),
                    floating: current.floating,
                };
                let Some(matched) = config.match_rule(&facts) else {
                    continue;
                };

                // A rule without `output` defers to `sticky-follow`, exactly as
                // it does when the window first becomes sticky.
                let pin = if matched.outputs.is_empty() {
                    Self::default_pin_for(config.sticky_follow(), &windows, &layout, *id)
                } else {
                    matched.outputs
                };

                if window.pin != pin {
                    new_pins.push((*id, pin));
                }
            }
        }

        let updated = new_pins.len();
        if updated > 0 {
            let mut state = self.state.lock().await;
            for (id, pin) in new_pins {
                if let Some(window) = state.sticky_windows.get_mut(&id) {
                    window.pin = pin;
                }
            }
            drop(state);
            self.persist().await;
        }

        Ok(updated)
    }

    /// The compositor's workspace layout.
    async fn layout(&self) -> Result<Layout> {
        Ok(Layout::new(self.niri.workspaces().await?))
    }

    /// Output a window is currently on, if any.
    fn window_output(windows: &[WindowInfo], layout: &Layout, window_id: u64) -> Option<String> {
        let workspace_id = windows
            .iter()
            .find(|window| window.id == window_id)
            .and_then(|window| window.workspace_id)?;
        layout.output_of(workspace_id).map(String::from)
    }

    /// Pin a window with no rule pin gets: follow focus, or stay on the output
    /// it currently sits on when the configuration asks for that. Shared by the
    /// two paths that decide a pin: a window becoming sticky, and a reload.
    fn default_pin_for(
        follow: StickyFollow,
        windows: &[WindowInfo],
        layout: &Layout,
        window_id: u64,
    ) -> Vec<String> {
        match follow {
            StickyFollow::Focused => Vec::new(),
            StickyFollow::OwnOutput => Self::window_output(windows, layout, window_id)
                .map(|output| vec![output])
                .unwrap_or_default(),
        }
    }

    /// [`Self::default_pin_for`] with the running configuration.
    async fn default_pin(
        &self,
        windows: &[WindowInfo],
        layout: &Layout,
        window_id: u64,
    ) -> Vec<String> {
        Self::default_pin_for(
            self.config().await.sticky_follow(),
            windows,
            layout,
            window_id,
        )
    }

    /// Name of the workspace staged windows are parked on.
    async fn stage_workspace_name(&self) -> String {
        self.config().await.stage_workspace().to_string()
    }

    /// Name of the workspace scratchpad windows are parked on.
    async fn scratchpad_workspace_name(&self) -> String {
        self.config().await.scratchpad_workspace().to_string()
    }

    /// Both parking areas, in the order they are kept at the end of an output
    /// (top to bottom).
    async fn parking_names(&self) -> [String; 2] {
        let config = self.config().await;
        [
            config.scratchpad_workspace().to_string(),
            config.stage_workspace().to_string(),
        ]
    }

    /// Size and position to put a window back with. Only a floating window has
    /// a position, and only its size is worth restoring.
    async fn capture_geometry(&self, window_id: u64) -> (Option<(i32, i32)>, Option<(f64, f64)>) {
        let Ok(windows) = self.windows().await else {
            return (None, None);
        };
        match windows.iter().find(|window| window.id == window_id) {
            Some(window) if window.floating => (window.size, window.position),
            _ => (None, None),
        }
    }

    /// Toggle a scratchpad.
    ///
    /// A configured scratchpad toggles its own window; one without matching
    /// fields, and `nsticky scratchpad` with no name, toggle whatever has
    /// focus. A second press brings back the window it parked, with its size
    /// and position; with nothing to show, the configured command is started.
    pub async fn toggle_scratchpad(&self, name: Option<&str>) -> Result<String> {
        let _operation = self.operations.lock().await;

        let (scratchpad, label) = {
            let config = self.config().await;
            match name {
                Some(name) => {
                    let Some(scratchpad) = config.scratchpad(name) else {
                        let known = config.scratchpad_names();
                        if known.is_empty() {
                            bail!("No [scratchpad.{name}] is configured");
                        }
                        bail!(
                            "Unknown scratchpad {name:?}; configured: {}",
                            known.join(", ")
                        );
                    };
                    (Some(scratchpad.clone()), name.to_string())
                }
                // No name: the focused window, no configuration needed.
                None => (None, "focused".to_string()),
            }
        };

        // A window this scratchpad parked before comes back, even when another
        // window has focus now: that is what makes it a toggle.
        if let Some(window_id) = self.scratchpad_window(&label).await {
            self.show_scratchpad(scratchpad.as_ref(), &label, window_id)
                .await?;
            return Ok(format!("Shown {label} ({window_id})"));
        }

        // Otherwise the configured window, or whatever is focused.
        let target = match &scratchpad {
            Some(scratchpad) if !scratchpad.follows_focus() => {
                let windows = self.windows().await?;
                windows
                    .iter()
                    .find(|window| {
                        scratchpad.matches(&WindowFacts {
                            app_id: window.app_id.as_deref(),
                            title: window.title.as_deref(),
                            floating: window.floating,
                        })
                    })
                    .map(|window| window.id)
            }
            _ => Some(self.active_window_id().await?),
        };

        // Park it on the scratchpad workspace, remembering where it was and how
        // big it was. A window parked on the stage by hand is taken over here:
        // one window belongs to one parking area, and the stage would otherwise
        // bring back a window the scratchpad owns.
        if let Some(window_id) = target {
            self.hide_scratchpad(&label, window_id).await?;
            return Ok(format!("Hidden {label} ({window_id})"));
        }

        // Nothing to toggle: start the configured command, if any.
        let Some(scratchpad) = scratchpad else {
            bail!("No window is focused");
        };
        let Some(command) = &scratchpad.spawn else {
            bail!("Nothing matches scratchpad {label:?} and it has no `spawn` command configured");
        };
        self.niri.spawn(command).await?;
        Ok(format!("Starting {label} ({})", command.join(" ")))
    }

    /// Where a window currently is, if niri reports it.
    async fn window_position(&self, window_id: u64) -> Option<(f64, f64)> {
        self.windows()
            .await
            .ok()?
            .iter()
            .find(|window| window.id == window_id)
            .and_then(|window| window.position)
    }

    /// Window this scratchpad parked, if it is still parked.
    async fn scratchpad_window(&self, label: &str) -> Option<u64> {
        let window_id = {
            let state = self.state.lock().await;
            state.scratchpad_slots.get(label).map(|window| window.id)
        }?;

        if self.is_scratchpad_window(window_id).await {
            return Some(window_id);
        }

        self.state.lock().await.scratchpad_slots.remove(label);
        None
    }

    /// Whether a window is parked by some scratchpad.
    async fn is_scratchpad_window(&self, window_id: u64) -> bool {
        let state = self.state.lock().await;
        state
            .scratchpad_slots
            .values()
            .any(|window| window.id == window_id)
    }

    /// Park a window on the scratchpad workspace, remembering how it was.
    async fn hide_scratchpad(&self, label: &str, window_id: u64) -> Result<()> {
        self.ensure_window_exists(window_id).await?;
        let (size, position) = self.capture_geometry(window_id).await;

        // Take it out of the stage if it happened to be parked there, keeping
        // what the stage knew about it.
        let record = {
            let mut state = self.state.lock().await;
            let staged = state.staged_windows.remove(&window_id);
            let sticky = state.sticky_windows.remove(&window_id);

            let (was_sticky, pin, staged_size, staged_position) = match staged {
                Some(staged) => (staged.was_sticky, staged.pin, staged.size, staged.position),
                None => (
                    sticky.is_some(),
                    sticky.map(|sticky| sticky.pin).unwrap_or_default(),
                    None,
                    None,
                ),
            };

            ScratchpadWindow {
                id: window_id,
                was_sticky,
                pin,
                size: staged_size.or(size),
                position: staged_position.or(position),
            }
        };

        let name = self.scratchpad_workspace_name().await;
        let workspace = self.parking_workspace_for(&name, window_id).await?;

        if let Err(e) = self.niri.move_window(window_id, workspace).await {
            // Roll back: it stays where it was.
            let mut state = self.state.lock().await;
            if record.was_sticky {
                state.sticky_windows.insert(
                    window_id,
                    StickyWindow {
                        pin: record.pin.clone(),
                    },
                );
            }
            return Err(e);
        }

        self.state
            .lock()
            .await
            .scratchpad_slots
            .insert(label.to_string(), record);

        // The stage may have lost its last window to this scratchpad.
        let stage_name = self.stage_workspace_name().await;
        self.release_parking_workspace(&stage_name, "a scratchpad took its window over")
            .await;

        self.persist().await;
        Ok(())
    }

    /// Bring a parked window back to the focused workspace: floating, with the
    /// geometry it had, and focused.
    async fn show_scratchpad(
        &self,
        scratchpad: Option<&Scratchpad>,
        label: &str,
        window_id: u64,
    ) -> Result<()> {
        let record = {
            let mut state = self.state.lock().await;
            state.scratchpad_slots.remove(label)
        };
        let (remembered_size, remembered_position) = record
            .as_ref()
            .map(|record| (record.size, record.position))
            .unwrap_or((None, None));

        let workspace_id = self.active_workspace_id().await?;
        if let Err(e) = self.niri.move_window(window_id, workspace_id).await {
            // Roll back: it stays parked.
            let mut state = self.state.lock().await;
            if let Some(record) = record {
                state.scratchpad_slots.insert(label.to_string(), record);
            }
            return Err(e);
        }

        if let Some(record) = &record
            && record.was_sticky
        {
            self.state.lock().await.sticky_windows.insert(
                window_id,
                StickyWindow {
                    pin: record.pin.clone(),
                },
            );
        }

        // A scratchpad floats by default; a remembered size and position win
        // over the configured ones, so it comes back exactly as it was.
        let float = scratchpad.is_none_or(|scratchpad| scratchpad.float);
        if float && let Err(e) = self.niri.float_window(window_id).await {
            tracing::warn!("Failed to float window {window_id}: {e:#}");
        }

        let width = remembered_size
            .map(|(width, _)| Size::Fixed(width))
            .or_else(|| scratchpad.and_then(|s| s.width).map(to_niri_size));
        let height = remembered_size
            .map(|(_, height)| Size::Fixed(height))
            .or_else(|| scratchpad.and_then(|s| s.height).map(to_niri_size));
        if (width.is_some() || height.is_some())
            && let Err(e) = self.niri.resize_window(window_id, width, height).await
        {
            tracing::warn!("Failed to size window {window_id}: {e:#}");
        }

        if let Some((x, y)) = remembered_position {
            if let Err(e) = self.niri.move_floating_window(window_id, x, y).await {
                tracing::warn!("Failed to move window {window_id}: {e:#}");
            }

            // niri places floating windows relative to the working area, below
            // bars and other struts, but reports them in workspace coordinates.
            // Ask where it landed and correct the difference, so the remembered
            // position holds whatever struts are in the way.
            if let Some((actual_x, actual_y)) = self.window_position(window_id).await {
                let (offset_x, offset_y) = (actual_x - x, actual_y - y);
                if (offset_x.abs() > 0.5 || offset_y.abs() > 0.5)
                    && let Err(e) = self
                        .niri
                        .move_floating_window(window_id, x - offset_x, y - offset_y)
                        .await
                {
                    tracing::warn!("Failed to place window {window_id}: {e:#}");
                }
            }
        }

        self.niri.focus_window(window_id).await?;

        // The window is back, so the scratchpad workspace may be empty now.
        let name = self.scratchpad_workspace_name().await;
        self.release_parking_workspace(&name, "the scratchpad window is back")
            .await;

        self.persist().await;
        Ok(())
    }

    /// Workspace id parked windows belong on, creating it if needed.
    ///
    /// niri has no hidden workspaces, and a named one never disappears on its
    /// own: declaring it in niri's config makes it permanent and shifts the
    /// dynamic workspace indices. So a parking area is only named while
    /// something is parked on it: take the empty workspace at the bottom of the
    /// window's output (the tail niri keeps anyway, so nothing appears in the
    /// middle of the strip), name it, and give the name back when the last
    /// window leaves.
    async fn parking_workspace_for(&self, name: &str, window_id: u64) -> Result<u64> {
        let workspaces = self.niri.workspaces().await?;

        if let Some(existing) = workspaces
            .iter()
            .find(|workspace| workspace.name.as_deref() == Some(name))
        {
            return Ok(existing.id);
        }

        let windows = self.windows().await?;
        let busy: HashSet<u64> = windows
            .iter()
            .filter_map(|window| window.workspace_id)
            .collect();
        let layout = Layout::new(workspaces);

        // Park on the output the window is on, or on the active workspace's
        // output when it has none.
        let output = Self::window_output(&windows, &layout, window_id)
            .or_else(|| {
                layout
                    .active_workspace_of_outputs()
                    .into_iter()
                    .next()
                    .map(|(output, _)| output)
            })
            .with_context(|| format!("No output available to create the {name:?} workspace on"))?;

        let target = layout
            .bottom_empty_workspace(&output, &busy)
            .with_context(|| format!("No empty workspace on {output} to turn into {name:?}"))?;

        self.niri
            .name_workspace(name, target)
            .await
            .with_context(|| format!("Failed to name workspace {target} as {name:?}"))?;
        tracing::debug!("Named workspace {target} on {output} as {name:?}");

        Ok(target)
    }

    /// Give a parking workspace its freedom back when nothing is parked on it.
    ///
    /// The stage respects `stage-keep-workspace`; the scratchpad is always
    /// released when its window is back, since a spot holding one window has no
    /// reason to stay in the strip.
    async fn release_parking_workspace(&self, name: &str, reason: &str) {
        let stage_name = self.stage_workspace_name().await;
        if name == stage_name && self.config().await.stage_keep_workspace() {
            return;
        }

        let empty = {
            let state = self.state.lock().await;
            if name == stage_name {
                state.staged_windows.is_empty()
            } else {
                state.scratchpad_slots.is_empty()
            }
        };
        if !empty {
            return;
        }

        let workspaces = match self.niri.workspaces().await {
            Ok(workspaces) => workspaces,
            Err(e) => {
                tracing::debug!("Not releasing {name:?} ({reason}): {e:#}");
                return;
            }
        };

        let Some(workspace) = workspaces
            .iter()
            .find(|workspace| workspace.name.as_deref() == Some(name))
        else {
            return;
        };

        if let Err(e) = self.niri.unname_workspace(workspace.id).await {
            tracing::debug!("Failed to release {name:?} ({reason}): {e:#}");
        } else {
            tracing::debug!("Released the {name:?} workspace ({reason})");
        }
    }

    /// Keep both parking workspaces at the end of their output, above the empty
    /// tail, so they never mix with the workspaces the user works on.
    ///
    /// The layout comes from the compositor, never from the event that
    /// triggered this, and the operation lock serializes the runs: moving the
    /// areas takes several calls, and acting on a state seen in the middle of
    /// them would make each move undo the previous one.
    pub async fn ensure_parking_order(&self) -> Result<()> {
        let _operation = self.operations.lock().await;
        let workspaces = self.niri.workspaces().await?;
        let windows = self.windows().await?;
        let occupied: HashSet<u64> = windows
            .iter()
            .filter_map(|window| window.workspace_id)
            .collect();
        let names = self.parking_names().await;
        let names: Vec<&str> = names.iter().map(String::as_str).collect();

        for (workspace_id, index) in parking_order_moves(&workspaces, &occupied, &names) {
            if let Err(e) = self.niri.move_workspace_to_index(workspace_id, index).await {
                tracing::debug!("Failed to move workspace {workspace_id} to index {index}: {e:#}");
            }
        }

        Ok(())
    }

    /// Every open window.
    async fn windows(&self) -> Result<Vec<WindowInfo>> {
        self.niri.windows().await
    }

    /// Ids of every open window.
    async fn window_ids(&self) -> Result<HashSet<u64>> {
        Ok(self
            .windows()
            .await?
            .into_iter()
            .map(|window| window.id)
            .collect())
    }

    /// Reject window ids the compositor does not know about.
    async fn ensure_window_exists(&self, window_id: u64) -> Result<()> {
        if !self.window_ids().await?.contains(&window_id) {
            bail!("Window not found in Niri");
        }
        Ok(())
    }

    /// Active workspace id, with the message shown to CLI users on failure.
    async fn active_workspace_id(&self) -> Result<u64> {
        self.niri
            .active_workspace()
            .await
            .context("Failed to get active workspace ID")
    }

    /// Focused window id, with the message shown to CLI users on failure.
    async fn active_window_id(&self) -> Result<u64> {
        self.niri
            .active_window()
            .await
            .context("Failed to get active window")
    }

    /// Add a window to the sticky list, pinning it according to the
    /// configuration. Returns whether it was newly added.
    pub async fn add_sticky_window(&self, window_id: u64) -> Result<bool> {
        let windows = self.windows().await?;
        if !windows.iter().any(|window| window.id == window_id) {
            bail!("Window not found in Niri");
        }
        let layout = self.layout().await?;
        let pin = self.default_pin(&windows, &layout, window_id).await;

        self.stick(window_id, pin).await
    }

    /// Add a window to the sticky list with an explicit output pin.
    async fn stick(&self, window_id: u64, pin: Vec<String>) -> Result<bool> {
        let mut state = self.state.lock().await;
        if state.staged_windows.contains_key(&window_id) {
            return Ok(false);
        }
        Ok(state
            .sticky_windows
            .insert(window_id, StickyWindow { pin })
            .is_none())
    }

    /// Remove a window from the sticky list. Returns whether it was present.
    pub async fn remove_sticky_window(&self, window_id: u64) -> Result<bool> {
        self.ensure_window_exists(window_id).await?;

        let mut state = self.state.lock().await;
        if state.staged_windows.contains_key(&window_id) {
            bail!("Window is in stage, cannot remove from sticky");
        }
        Ok(state.sticky_windows.remove(&window_id).is_some())
    }

    /// Sticky windows that still exist.
    pub async fn list_sticky_windows(&self) -> Result<Vec<u64>> {
        let snapshot: Vec<u64> = {
            let state = self.state.lock().await;
            state.sticky_windows.keys().copied().collect()
        };
        let window_ids = self.window_ids().await?;
        Ok(snapshot
            .into_iter()
            .filter(|id| window_ids.contains(id))
            .collect())
    }

    /// Toggle the focused window: staged windows move back to the active
    /// workspace and become sticky; otherwise sticky state is flipped.
    /// Returns whether the window ended up sticky.
    pub async fn toggle_active_window(&self) -> Result<bool> {
        let window_id = self.active_window_id().await?;
        if !self.window_ids().await?.contains(&window_id) {
            bail!("Active window not found in Niri");
        }

        let is_staged = {
            let state = self.state.lock().await;
            state.staged_windows.contains_key(&window_id)
        };

        if is_staged {
            let workspace_id = self.active_workspace_id().await?;
            self.niri.move_window(window_id, workspace_id).await?;
            let mut state = self.state.lock().await;
            let staged = state.staged_windows.remove(&window_id);
            let pin = staged.map(|staged| staged.pin).unwrap_or_default();
            state.sticky_windows.insert(window_id, StickyWindow { pin });
            return Ok(true);
        }

        {
            let mut state = self.state.lock().await;
            if state.sticky_windows.remove(&window_id).is_some() {
                return Ok(false);
            }
        }

        self.add_sticky_window(window_id).await?;
        Ok(true)
    }

    /// Toggle sticky state of every window with this exact app id.
    pub async fn toggle_by_appid(&self, appid: &str) -> Result<BatchOutcome> {
        let windows = self.windows().await?;
        let ids = ids_by_appid(&windows, appid);
        if ids.is_empty() {
            bail!("No window found with appid {appid}");
        }
        self.toggle_sticky_windows(&ids).await
    }

    /// Toggle sticky state of every window whose title contains this string.
    pub async fn toggle_by_title(&self, title: &str) -> Result<BatchOutcome> {
        let windows = self.windows().await?;
        let ids = ids_by_title(&windows, title);
        if ids.is_empty() {
            bail!("No window found with title {title}");
        }
        self.toggle_sticky_windows(&ids).await
    }

    async fn toggle_sticky_windows(&self, ids: &[u64]) -> Result<BatchOutcome> {
        let current_ws_id = self.active_workspace_id().await?;
        let windows = self.windows().await?;
        let layout = self.layout().await?;
        let mut outcome = BatchOutcome::default();

        for id in ids {
            let (is_staged, is_sticky) = {
                let state = self.state.lock().await;
                (
                    state.staged_windows.contains_key(id),
                    state.sticky_windows.contains_key(id),
                )
            };

            match (is_staged, is_sticky) {
                (true, _) => {
                    if let Err(e) = self.niri.move_window(*id, current_ws_id).await {
                        tracing::warn!("Failed to move window {id}: {e:#}");
                        outcome.failed.push(*id);
                        continue;
                    }

                    let mut state = self.state.lock().await;
                    let staged = state.staged_windows.remove(id);
                    if let Some(staged) = staged
                        && staged.was_sticky
                    {
                        state.sticky_windows.insert(
                            *id,
                            StickyWindow {
                                pin: staged.pin.clone(),
                            },
                        );
                    }
                }
                (false, true) => {
                    self.state.lock().await.sticky_windows.remove(id);
                }
                (false, false) => {
                    let pin = self.default_pin(&windows, &layout, *id).await;
                    self.state
                        .lock()
                        .await
                        .sticky_windows
                        .insert(*id, StickyWindow { pin });
                }
            }
            outcome.succeeded.push(*id);
        }
        Ok(outcome)
    }

    /// Move a window to the stage workspace, remembering whether it was sticky.
    pub async fn stage_window(&self, window_id: u64) -> Result<()> {
        self.ensure_window_exists(window_id).await?;
        let (size, position) = self.capture_geometry(window_id).await;
        let name = self.stage_workspace_name().await;

        let staged = {
            let mut state = self.state.lock().await;

            if state.staged_windows.contains_key(&window_id) {
                return Ok(());
            }

            let previous = state.sticky_windows.remove(&window_id);
            let staged = StagedWindow {
                was_sticky: previous.is_some(),
                pin: previous.map(|sticky| sticky.pin).unwrap_or_default(),
                size,
                position,
            };
            state.staged_windows.insert(window_id, staged.clone());
            staged
        };

        let stage_workspace = match self.parking_workspace_for(&name, window_id).await {
            Ok(workspace) => workspace,
            Err(e) => {
                // Nothing was parked: undo the bookkeeping and report.
                let mut state = self.state.lock().await;
                state.staged_windows.remove(&window_id);
                if staged.was_sticky {
                    state.sticky_windows.insert(
                        window_id,
                        StickyWindow {
                            pin: staged.pin.clone(),
                        },
                    );
                }
                return Err(e);
            }
        };

        if let Err(e) = self.niri.move_window(window_id, stage_workspace).await {
            // Roll the bookkeeping back: the compositor refused the move.
            let mut state = self.state.lock().await;
            state.staged_windows.remove(&window_id);
            if staged.was_sticky {
                state.sticky_windows.insert(
                    window_id,
                    StickyWindow {
                        pin: staged.pin.clone(),
                    },
                );
            }
            return Err(e);
        }

        Ok(())
    }

    #[cfg(test)]
    async fn pin_of(&self, window_id: u64) -> Vec<String> {
        self.state
            .lock()
            .await
            .sticky_windows
            .get(&window_id)
            .map(|window| window.pin.clone())
            .unwrap_or_default()
    }

    pub async fn is_window_staged(&self, window_id: u64) -> bool {
        self.state
            .lock()
            .await
            .staged_windows
            .contains_key(&window_id)
    }

    /// Stage or unstage every window with this exact app id.
    pub async fn toggle_stage_by_appid(
        &self,
        appid: &str,
        workspace_id: u64,
    ) -> Result<BatchOutcome> {
        let windows = self.windows().await?;
        let ids = ids_by_appid(&windows, appid);
        if ids.is_empty() {
            bail!("No window found with appid {appid}");
        }
        self.toggle_staged_windows(&ids, workspace_id).await
    }

    /// Stage or unstage every window whose title contains this string.
    pub async fn toggle_stage_by_title(
        &self,
        title: &str,
        workspace_id: u64,
    ) -> Result<BatchOutcome> {
        let windows = self.windows().await?;
        let ids = ids_by_title(&windows, title);
        if ids.is_empty() {
            bail!("No window found with title {title}");
        }
        self.toggle_staged_windows(&ids, workspace_id).await
    }

    async fn toggle_staged_windows(&self, ids: &[u64], workspace_id: u64) -> Result<BatchOutcome> {
        let mut outcome = BatchOutcome::default();

        for id in ids {
            let result = if self.is_window_staged(*id).await {
                self.unstage_window(*id, workspace_id).await
            } else {
                self.stage_window(*id).await
            };

            match result {
                Ok(()) => outcome.succeeded.push(*id),
                Err(e) => {
                    tracing::warn!("Failed to toggle window {id}: {e:#}");
                    outcome.failed.push(*id);
                }
            }
        }
        Ok(outcome)
    }

    /// Stage every sticky window.
    pub async fn stage_all_windows(&self) -> Result<BatchOutcome> {
        let window_ids = self.window_ids().await?;
        let sticky_ids: Vec<u64> = {
            let state = self.state.lock().await;
            state
                .sticky_windows
                .keys()
                .copied()
                .filter(|id| window_ids.contains(id))
                .collect()
        };

        let mut outcome = BatchOutcome::default();
        let stage_name = self.stage_workspace_name().await;
        let mut stage_workspace = None;

        for id in sticky_ids {
            let (size, position) = self.capture_geometry(id).await;
            let staged = {
                let mut state = self.state.lock().await;
                let previous = state.sticky_windows.remove(&id);
                let staged = StagedWindow {
                    was_sticky: previous.is_some(),
                    pin: previous.map(|sticky| sticky.pin).unwrap_or_default(),
                    size,
                    position,
                };
                state.staged_windows.insert(id, staged.clone());
                staged
            };

            let destination = match stage_workspace {
                Some(workspace) => Ok(workspace),
                None => match self.parking_workspace_for(&stage_name, id).await {
                    Ok(workspace) => {
                        stage_workspace = Some(workspace);
                        Ok(workspace)
                    }
                    Err(e) => Err(e),
                },
            };

            let moved = match destination {
                Ok(workspace) => self.niri.move_window(id, workspace).await,
                Err(e) => Err(e),
            };

            if let Err(e) = moved {
                tracing::error!("Failed to move window {id} to the stage workspace: {e}");
                let mut state = self.state.lock().await;
                state.staged_windows.remove(&id);
                if staged.was_sticky {
                    state.sticky_windows.insert(
                        id,
                        StickyWindow {
                            pin: staged.pin.clone(),
                        },
                    );
                }
                outcome.failed.push(id);
            } else {
                outcome.succeeded.push(id);
            }
        }

        Ok(outcome)
    }

    pub async fn list_staged_windows(&self) -> Result<Vec<u64>> {
        let state = self.state.lock().await;
        Ok(state.staged_windows.keys().copied().collect())
    }

    /// Move a staged window back to `workspace_id`.
    pub async fn unstage_window(&self, window_id: u64, workspace_id: u64) -> Result<()> {
        self.ensure_window_exists(window_id).await?;

        let staged = {
            let mut state = self.state.lock().await;
            match state.staged_windows.remove(&window_id) {
                Some(staged) => staged,
                None => bail!("Window is not in staged list"),
            }
        };

        if let Err(e) = self.niri.move_window(window_id, workspace_id).await {
            // The window stays on the stage, so it stays out of the sticky set
            // too: a window is either sticky or staged, never both.
            self.state
                .lock()
                .await
                .staged_windows
                .insert(window_id, staged);
            return Err(e);
        }

        {
            let mut state = self.state.lock().await;
            if staged.was_sticky {
                state.sticky_windows.insert(
                    window_id,
                    StickyWindow {
                        pin: staged.pin.clone(),
                    },
                );
            }
        }

        let stage_name = self.stage_workspace_name().await;
        self.release_parking_workspace(&stage_name, "nothing parked any more")
            .await;
        Ok(())
    }

    /// Move every staged window back.
    pub async fn unstage_all_windows(&self, workspace_id: u64) -> Result<BatchOutcome> {
        let previously_staged: Vec<(u64, StagedWindow)> = {
            let mut state = self.state.lock().await;
            if state.staged_windows.is_empty() {
                return Ok(BatchOutcome::default());
            }
            std::mem::take(&mut state.staged_windows)
                .into_iter()
                .collect()
        };

        let window_ids = self.window_ids().await?;
        let mut restored = Vec::new();
        let mut failed = Vec::new();

        for (id, staged) in previously_staged {
            // A window that closed while staged is forgotten, not failed.
            if !window_ids.contains(&id) {
                continue;
            }

            match self.niri.move_window(id, workspace_id).await {
                Ok(()) => restored.push((id, staged)),
                Err(e) => {
                    tracing::error!("Failed to move window {id} to workspace {workspace_id}: {e}");
                    self.state.lock().await.staged_windows.insert(id, staged);
                    failed.push(id);
                }
            }
        }

        let outcome = BatchOutcome {
            succeeded: restored.iter().map(|(id, _)| *id).collect(),
            failed,
        };
        {
            let mut state = self.state.lock().await;
            for (id, staged) in restored {
                if staged.was_sticky {
                    state.sticky_windows.insert(
                        id,
                        StickyWindow {
                            pin: staged.pin.clone(),
                        },
                    );
                }
            }
        }

        let stage_name = self.stage_workspace_name().await;
        self.release_parking_workspace(&stage_name, "nothing parked any more")
            .await;
        Ok(outcome)
    }

    /// Bring every sticky window along when the user switches workspace.
    pub async fn handle_workspace_activation(&self, ws_id: u64) -> Result<()> {
        let _operation = self.operations.lock().await;

        let windows = match self.windows().await {
            Ok(windows) => windows,
            Err(e) => {
                tracing::error!("Failed to get window list: {e:?}");
                return Ok(());
            }
        };
        let layout = match self.layout().await {
            Ok(layout) => layout,
            Err(e) => {
                tracing::error!("Failed to get the workspace layout: {e:?}");
                return Ok(());
            }
        };

        let window_ids: HashSet<u64> = windows.iter().map(|window| window.id).collect();
        let sticky_windows: Vec<(u64, Vec<String>)> = {
            let mut state = self.state.lock().await;
            state.sticky_windows.retain(|id, _| window_ids.contains(id));
            state
                .sticky_windows
                .iter()
                .map(|(id, window)| (*id, window.pin.clone()))
                .collect()
        };

        for (win_id, pin) in sticky_windows {
            let current_output = Self::window_output(&windows, &layout, win_id);
            let Some(destination) = pin_target(&pin, current_output.as_deref(), &layout, ws_id)
            else {
                continue;
            };

            if let Err(e) = self.niri.move_window(win_id, destination).await {
                tracing::error!("Failed to move window {win_id}: {e:?}");
            }
        }

        Ok(())
    }

    /// Apply the configured rule, if any, to a window that opened or changed.
    ///
    /// Sticky rules are declarative: a matching window is (re-)added to the
    /// sticky list whenever it changes. Stage rules fire once per window and
    /// daemon run, so restoring a parked window is not immediately undone by
    /// the next event it produces.
    pub async fn handle_window_opened_or_changed(
        &self,
        id: u64,
        app_id: Option<String>,
        title: Option<String>,
        floating: bool,
    ) -> Result<()> {
        let _operation = self.operations.lock().await;

        let facts = WindowFacts {
            app_id: app_id.as_deref(),
            title: title.as_deref(),
            floating,
        };

        let matched = self.config().await.match_rule(&facts);

        match matched.as_ref().map(|matched| matched.action) {
            Some(WindowAction::Sticky) => {
                let pin = match matched.as_ref().filter(|m| !m.outputs.is_empty()) {
                    Some(matched) => matched.outputs.clone(),
                    None => {
                        let windows = self.windows().await?;
                        let layout = self.layout().await?;
                        self.default_pin(&windows, &layout, id).await
                    }
                };
                tracing::info!(
                    "Auto-sticky window {id} ({app_id:?}, rule {})",
                    matched.as_ref().map(|m| m.id.as_str()).unwrap_or("?")
                );
                self.stick(id, pin).await?;
            }
            Some(WindowAction::Stage) => {
                let already_staged = {
                    let mut state = self.state.lock().await;
                    !state.auto_staged.insert(id)
                };
                if already_staged {
                    return Ok(());
                }

                tracing::info!("Auto-stage window {id} ({app_id:?})");
                if let Err(e) = self.stage_window(id).await {
                    // Let a later event retry: the window is still not parked.
                    self.state.lock().await.auto_staged.remove(&id);
                    return Err(e);
                }
            }
            None => {}
        }

        self.persist().await;
        Ok(())
    }

    pub async fn handle_window_closed(&self, id: u64) -> Result<()> {
        let _operation = self.operations.lock().await;
        let was_staged = self.is_window_staged(id).await;
        let was_scratchpad = self.is_scratchpad_window(id).await;
        self.remove_window_unconditionally(id).await?;

        if was_staged || was_scratchpad {
            let (stage_name, scratchpad_name) = (
                self.stage_workspace_name().await,
                self.scratchpad_workspace_name().await,
            );
            self.release_parking_workspace(&stage_name, "a parked window closed")
                .await;
            self.release_parking_workspace(&scratchpad_name, "a parked window closed")
                .await;
        }

        self.persist().await;
        Ok(())
    }

    /// Drop every trace of a window, ignoring whether it was sticky or staged.
    pub async fn remove_window_unconditionally(&self, window_id: u64) -> Result<()> {
        let mut state = self.state.lock().await;
        state.sticky_windows.remove(&window_id);
        state.staged_windows.remove(&window_id);
        state.auto_staged.remove(&window_id);
        state
            .scratchpad_slots
            .retain(|_, window| window.id != window_id);
        Ok(())
    }

    pub async fn handle_request(&self, request: Request) -> Response {
        // These take the operation lock themselves, and it is not reentrant, so
        // they are handled before dispatch takes it.
        match &request {
            Request::Reload => {
                return match self.reload_from_disk().await {
                    Ok(message) => Response::success(message),
                    Err(e) => Response::error(e),
                };
            }
            Request::Scratchpad { name } => {
                let name = name.clone();
                return match self.toggle_scratchpad(name.as_deref()).await {
                    Ok(message) => Response::success(message),
                    Err(e) => Response::error(e),
                };
            }
            _ => {}
        }

        let _operation = self.operations.lock().await;
        let response = self.dispatch(request).await;
        self.persist().await;
        response
    }

    async fn dispatch(&self, request: Request) -> Response {
        match request {
            Request::Add { window_id } => {
                reply(self.add_sticky_window(window_id).await, |is_new| {
                    if is_new {
                        "Added"
                    } else {
                        "Already in sticky list"
                    }
                    .to_string()
                })
            }

            Request::Remove { window_id } => {
                reply(self.remove_sticky_window(window_id).await, |was_present| {
                    if was_present {
                        "Removed"
                    } else {
                        "Not in sticky list"
                    }
                    .to_string()
                })
            }

            Request::List => reply_json(self.list_sticky_windows().await),

            Request::ToggleActive => reply(self.toggle_active_window().await, |added| {
                if added {
                    "Added active window to sticky"
                } else {
                    "Removed active window from sticky"
                }
                .to_string()
            }),

            Request::ToggleAppid { appid } => {
                reply(self.toggle_by_appid(&appid).await, |outcome| {
                    outcome.message("Toggled", "window(s)")
                })
            }

            Request::ToggleTitle { title } => {
                reply(self.toggle_by_title(&title).await, |outcome| {
                    outcome.message("Toggled", "window(s)")
                })
            }

            Request::StageList => reply_json(self.list_staged_windows().await),

            Request::Stage { window_id } => reply(self.stage_window(window_id).await, |()| {
                "Staged window".to_string()
            }),

            Request::Unstage { window_id } => {
                let workspace_id = match self.active_workspace_id().await {
                    Ok(id) => id,
                    Err(e) => return Response::error(e),
                };
                reply(self.unstage_window(window_id, workspace_id).await, |()| {
                    "Unstaged window".to_string()
                })
            }

            Request::StageToggleActive => {
                let window_id = match self.active_window_id().await {
                    Ok(id) => id,
                    Err(e) => return Response::error(e),
                };

                if self.is_window_staged(window_id).await {
                    let workspace_id = match self.active_workspace_id().await {
                        Ok(id) => id,
                        Err(e) => return Response::error(e),
                    };
                    reply(self.unstage_window(window_id, workspace_id).await, |()| {
                        "Unstaged active window".to_string()
                    })
                } else {
                    reply(self.stage_window(window_id).await, |()| {
                        "Staged active window".to_string()
                    })
                }
            }

            Request::StageToggleAppid { appid } => {
                let workspace_id = match self.active_workspace_id().await {
                    Ok(id) => id,
                    Err(e) => return Response::error(e),
                };
                reply(
                    self.toggle_stage_by_appid(&appid, workspace_id).await,
                    |outcome| outcome.message("Toggled", "window(s)"),
                )
            }

            Request::StageToggleTitle { title } => {
                let workspace_id = match self.active_workspace_id().await {
                    Ok(id) => id,
                    Err(e) => return Response::error(e),
                };
                reply(
                    self.toggle_stage_by_title(&title, workspace_id).await,
                    |outcome| outcome.message("Toggled", "window(s)"),
                )
            }

            Request::StageAll => reply(self.stage_all_windows().await, |outcome| {
                outcome.message("Staged", "windows")
            }),

            Request::UnstageAll => {
                let workspace_id = match self.active_workspace_id().await {
                    Ok(id) => id,
                    Err(e) => return Response::error(e),
                };
                reply(self.unstage_all_windows(workspace_id).await, |outcome| {
                    outcome.message("Unstaged", "windows")
                })
            }

            Request::Windows => reply_json(self.windows().await),

            Request::Scratchpad { .. } => {
                unreachable!("Scratchpad is handled by the caller")
            }

            // Handled by the caller before dispatch, like `Scratchpad`.
            Request::Reload => unreachable!("Reload is handled by the caller"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::niri::fake::{Call, FakeNiri};
    use crate::state_store::MemoryStorage;

    /// Two terminals, a browser and a music player.
    const WINDOWS: &[(u64, &str, &str)] = &[
        (1, "foot", "Terminal"),
        (2, "foot", "Terminal"),
        (3, "firefox", "Inbox - Gmail"),
        (4, "Spotify", "Spotify Premium"),
    ];

    /// A configuration file tests can rewrite, removed when the test ends.
    struct TempConfig(PathBuf);

    impl TempConfig {
        fn new() -> Self {
            static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "nsticky-reload-test-{}-{seq}.toml",
                std::process::id()
            )))
        }

        fn write(&self, content: &str) {
            std::fs::write(&self.0, content).unwrap();
        }

        fn path(&self) -> PathBuf {
            self.0.clone()
        }
    }

    impl Drop for TempConfig {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn storage() -> Arc<MemoryStorage> {
        Arc::new(MemoryStorage::new())
    }

    const SCRATCHPAD_CONFIG: &str = r#"
stage-workspace = "stage"

[scratchpad.term]
app-id = "foot"
title = "dropdown-terminal"
spawn = ["foot", "--app-id", "foot", "--title", "dropdown-terminal"]
width = "60%"
height = "400"
"#;

    /// Run something with a deadline, so a lock mistake fails the test, not the suite.
    async fn with_deadline<T>(future: impl Future<Output = T>) -> T {
        match tokio::time::timeout(std::time::Duration::from_secs(5), future).await {
            Ok(value) => value,
            Err(_) => panic!("operation did not finish within 5s (deadlock?)"),
        }
    }

    /// A compositor with one output, an active workspace and a named stage workspace.
    fn with_layout() -> Arc<FakeNiri> {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        niri.set_workspaces(&[(1, "one", "DP-1"), (2, "stage", "DP-1"), (3, "", "DP-1")]);
        niri.set_active_workspace(1);
        niri.set_workspace_active(1, true);
        niri.set_window_workspace(1, 1);
        niri.set_window_workspace(2, 1);
        niri.set_window_workspace(3, 1);
        niri.set_window_workspace(4, 1);
        niri
    }

    fn business(niri: &Arc<FakeNiri>) -> BusinessLogic {
        BusinessLogic::new(
            Config::default(),
            Config::default_config_path(),
            niri.clone(),
            storage(),
        )
    }

    fn business_with_config(niri: &Arc<FakeNiri>, toml: &str) -> BusinessLogic {
        BusinessLogic::new(
            Config::from_toml(toml).unwrap(),
            Config::default_config_path(),
            niri.clone(),
            storage(),
        )
    }

    /// Business logic plus the storage it writes to, so tests can inspect it.
    fn business_with_storage(
        niri: &Arc<FakeNiri>,
        toml: &str,
        storage: &Arc<MemoryStorage>,
    ) -> BusinessLogic {
        BusinessLogic::new(
            Config::from_toml(toml).unwrap_or_default(),
            Config::default_config_path(),
            niri.clone(),
            storage.clone(),
        )
    }

    async fn staged(business: &BusinessLogic, id: u64) -> bool {
        business.is_window_staged(id).await
    }

    /// Whether a scratchpad parked this window (a separate area from the stage).
    async fn parked(business: &BusinessLogic, id: u64) -> bool {
        business.is_scratchpad_window(id).await
    }

    async fn sticky(business: &BusinessLogic, id: u64) -> bool {
        business.state.lock().await.sticky_windows.contains_key(&id)
    }

    #[test]
    fn test_ids_by_appid_and_title() {
        let windows: Vec<WindowInfo> = WINDOWS
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

        assert_eq!(ids_by_appid(&windows, "foot"), vec![1, 2]);
        assert_eq!(ids_by_appid(&windows, "Firefox"), Vec::<u64>::new());
        assert_eq!(ids_by_title(&windows, "Gmail"), vec![3]);
        assert_eq!(ids_by_title(&windows, "Terminal"), vec![1, 2]);
        assert_eq!(ids_by_title(&windows, "missing"), Vec::<u64>::new());
    }

    #[tokio::test]
    async fn test_stage_window_moves_to_stage_and_marks_staged() {
        let niri = with_layout();
        let business = business(&niri);

        business.add_sticky_window(1).await.unwrap();
        business.stage_window(1).await.unwrap();

        assert!(staged(&business, 1).await);
        assert!(!sticky(&business, 1).await);
        assert_eq!(niri.moved_to_id(2), vec![1]);
    }

    #[tokio::test]
    async fn test_staging_creates_the_configured_workspace_on_demand() {
        // The layout has a workspace in use and the empty tail niri keeps; the
        // config asks for "parking", which does not exist yet: nsticky names the
        // empty workspace at the bottom instead of requiring it in niri's config.
        let niri = with_layout();
        niri.set_workspaces(&[(1, "one", "DP-1"), (2, "", "DP-1")]);
        let business = business_with_config(&niri, "stage-workspace = \"parking\"\n");

        business.stage_window(1).await.unwrap();

        let names = niri.named_workspaces();
        assert_eq!(
            names,
            vec![(1, "one".to_string()), (2, "parking".to_string())],
            "the tail took the configured name and nothing else moved"
        );
        assert_eq!(niri.moved_to_id(2), vec![1], "the window moved to its id");
    }

    #[tokio::test]
    async fn test_staging_does_not_rename_an_existing_stage_workspace() {
        let niri = with_layout();
        let business = business(&niri);

        business.stage_window(1).await.unwrap();

        assert!(
            niri.workspace_naming().is_empty(),
            "the stage workspace was already named: nothing to do, and no focus stolen"
        );
        assert_eq!(niri.moved_to_id(2), vec![1]);
        let names = niri.named_workspaces();
        assert!(
            names.contains(&(2, "stage".to_string())),
            "the stage workspace keeps its name: {names:?}"
        );
    }

    #[tokio::test]
    async fn test_the_stage_workspace_is_released_when_the_last_window_leaves() {
        let niri = with_layout();
        let business = business(&niri);
        for id in [1, 3] {
            business.stage_window(id).await.unwrap();
        }

        business.unstage_window(1, 1).await.unwrap();
        assert!(
            niri.workspace_naming().is_empty(),
            "a window is still parked: the workspace stays"
        );

        business.unstage_window(3, 1).await.unwrap();
        assert_eq!(
            niri.workspace_naming().len(),
            1,
            "the name is given back once nothing is parked: {:?}",
            niri.workspace_naming()
        );
        assert!(niri.workspace_naming()[0].contains("unname"));
    }

    #[tokio::test]
    async fn test_closing_the_last_parked_window_releases_the_workspace() {
        let niri = with_layout();
        let business = business(&niri);
        business.stage_window(1).await.unwrap();

        business.handle_window_closed(1).await.unwrap();

        assert_eq!(niri.workspace_naming().len(), 1);
        assert!(niri.workspace_naming()[0].contains("unname"));
    }

    #[tokio::test]
    async fn test_stage_workspace_is_kept_when_asked_for() {
        let niri = with_layout();
        let business = business_with_config(&niri, "stage-keep-workspace = true\n");
        business.stage_window(1).await.unwrap();

        business.unstage_window(1, 1).await.unwrap();

        assert!(
            niri.workspace_naming().is_empty(),
            "the configuration asked to keep the workspace: {:?}",
            niri.workspace_naming()
        );
    }

    #[tokio::test]
    async fn test_stage_window_rolls_back_when_the_move_fails() {
        let niri = with_layout();
        let business = business(&niri);
        business.add_sticky_window(1).await.unwrap();
        niri.fail_move(1);

        let error = business.stage_window(1).await.unwrap_err();

        assert!(error.to_string().contains("refused"), "{error}");
        assert!(!staged(&business, 1).await, "must not stay staged");
        assert!(sticky(&business, 1).await, "must stay sticky");
        assert!(niri.moves().is_empty());
    }

    #[tokio::test]
    async fn test_unstage_window_restores_sticky_state() {
        let niri = with_layout();
        let business = business(&niri);
        business.add_sticky_window(1).await.unwrap();
        business.stage_window(1).await.unwrap();
        niri.set_active_workspace(42);

        business.unstage_window(1, 42).await.unwrap();

        assert!(!staged(&business, 1).await);
        assert!(sticky(&business, 1).await, "was sticky before staging");
        assert_eq!(niri.moves().last().unwrap().1, 42);
    }

    #[tokio::test]
    async fn test_unstage_window_keeps_plain_windows_non_sticky() {
        let niri = with_layout();
        let business = business(&niri);
        business.stage_window(2).await.unwrap();

        business.unstage_window(2, 42).await.unwrap();

        assert!(!staged(&business, 2).await);
        assert!(!sticky(&business, 2).await, "never was sticky");
    }

    #[tokio::test]
    async fn test_unstage_window_rolls_back_when_the_move_fails() {
        let niri = with_layout();
        let business = business(&niri);
        business.add_sticky_window(1).await.unwrap();
        business.stage_window(1).await.unwrap();
        niri.fail_move(1);

        assert!(business.unstage_window(1, 42).await.is_err());

        assert!(staged(&business, 1).await, "must stay staged");
        assert!(
            !sticky(&business, 1).await,
            "a staged window must not be sticky at the same time"
        );
    }

    #[tokio::test]
    async fn test_unstage_window_rejects_windows_that_are_not_staged() {
        let niri = with_layout();
        let business = business(&niri);

        let error = business.unstage_window(1, 42).await.unwrap_err();

        assert!(error.to_string().contains("not in staged list"), "{error}");
    }

    #[tokio::test]
    async fn test_stage_all_windows_counts_only_successful_moves() {
        let niri = with_layout();
        let business = business(&niri);
        for id in [1, 2, 3] {
            business.add_sticky_window(id).await.unwrap();
        }
        niri.fail_move(2);

        let outcome = business.stage_all_windows().await.unwrap();

        assert_eq!(outcome.count(), 2);
        assert_eq!(outcome.failed, vec![2], "the failed move is reported");
        assert!(staged(&business, 1).await);
        assert!(!staged(&business, 2).await, "failed move is rolled back");
        assert!(sticky(&business, 2).await, "failed move keeps sticky state");
        assert!(staged(&business, 3).await);
        assert!(!sticky(&business, 1).await);
        assert!(!sticky(&business, 3).await);
    }

    #[tokio::test]
    async fn test_unstage_all_windows_keeps_failed_windows_staged() {
        let niri = with_layout();
        let business = business(&niri);
        business.add_sticky_window(1).await.unwrap();
        business.add_sticky_window(2).await.unwrap();
        business.stage_all_windows().await.unwrap();
        niri.fail_move(1);

        let outcome = business.unstage_all_windows(42).await.unwrap();

        assert_eq!(outcome.count(), 1);
        assert_eq!(outcome.failed, vec![1]);
        assert!(staged(&business, 1).await, "failed unstage stays staged");
        assert!(!staged(&business, 2).await);
        assert!(sticky(&business, 2).await, "restored to sticky");
    }

    #[tokio::test]
    async fn test_unstage_all_windows_forgets_windows_that_vanished() {
        let niri = with_layout();
        let business = business(&niri);
        for id in [1, 2] {
            business.add_sticky_window(id).await.unwrap();
        }
        business.stage_all_windows().await.unwrap();
        // Window 1 closed while staged.
        niri.set_windows(&[(2, "foot", "Terminal"), (3, "firefox", "Gmail")]);

        let outcome = business.unstage_all_windows(42).await.unwrap();

        assert_eq!(outcome.count(), 1);
        assert!(
            outcome.failed.is_empty(),
            "a window that closed is forgotten, not failed: {:?}",
            outcome.failed
        );
        assert!(!staged(&business, 1).await);
        assert!(!sticky(&business, 1).await);
        assert_eq!(business.list_staged_windows().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_toggle_active_staged_window_returns_to_the_active_workspace() {
        let niri = with_layout();
        let business = business(&niri);
        niri.set_active_window(3);
        niri.set_active_workspace(27);
        business.stage_window(3).await.unwrap();

        let is_sticky = business.toggle_active_window().await.unwrap();

        assert!(is_sticky);
        assert!(!staged(&business, 3).await);
        assert!(sticky(&business, 3).await);
        assert_eq!(niri.moves().last().unwrap().1, 27);
    }

    #[tokio::test]
    async fn test_toggle_active_window_flips_sticky_state() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = business(&niri);
        niri.set_active_window(1);

        assert!(business.toggle_active_window().await.unwrap());
        assert!(sticky(&business, 1).await);
        assert!(!business.toggle_active_window().await.unwrap());
        assert!(!sticky(&business, 1).await);
    }

    #[tokio::test]
    async fn test_toggle_by_appid_requires_a_match() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = business(&niri);

        let error = business.toggle_by_appid("nope").await.unwrap_err();
        assert!(error.to_string().contains("No window found"), "{error}");

        assert_eq!(business.toggle_by_appid("foot").await.unwrap().count(), 2);
        assert!(sticky(&business, 1).await);
        assert!(sticky(&business, 2).await);
    }

    #[tokio::test]
    async fn test_toggle_by_title_matches_substrings() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = business(&niri);

        assert_eq!(business.toggle_by_title("Gmail").await.unwrap().count(), 1);
        assert!(sticky(&business, 3).await);
    }

    #[tokio::test]
    async fn test_remove_sticky_window_conflicts_with_stage() {
        let niri = with_layout();
        let business = business(&niri);
        business.add_sticky_window(1).await.unwrap();
        business.stage_window(1).await.unwrap();

        let error = business.remove_sticky_window(1).await.unwrap_err();
        assert!(
            error.to_string().contains("cannot remove from sticky"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn test_add_sticky_window_rejects_unknown_windows() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = business(&niri);

        let error = business.add_sticky_window(999).await.unwrap_err();
        assert!(error.to_string().contains("not found in Niri"), "{error}");
    }

    #[tokio::test]
    async fn test_list_sticky_windows_drops_closed_windows() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = business(&niri);
        business.add_sticky_window(1).await.unwrap();
        business.add_sticky_window(2).await.unwrap();
        niri.set_windows(&[(1, "foot", "Terminal")]);

        assert_eq!(business.list_sticky_windows().await.unwrap(), vec![1]);
    }

    /// Two monitors: workspace 1 is active on DP-1, workspace 2 on DP-2.
    fn two_monitors() -> Arc<FakeNiri> {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        niri.set_workspaces(&[(1, "one", "DP-1"), (3, "three", "DP-1"), (2, "two", "DP-2")]);
        niri.set_workspace_active(1, true);
        niri.set_workspace_active(2, true);
        niri
    }

    #[tokio::test]
    async fn test_pinned_window_only_follows_its_own_output() {
        let niri = two_monitors();
        niri.set_window_workspace(1, 2); // window 1 lives on DP-2
        let business = business_with_config(
            &niri,
            r#"
[sticky.discord]
app-id = "foot"
output = "DP-2"
"#,
        );
        business
            .handle_window_opened_or_changed(1, Some("foot".into()), None, false)
            .await
            .unwrap();
        assert_eq!(business.pin_of(1).await, vec!["DP-2".to_string()]);

        // A workspace on the other monitor becomes focused: leave it alone.
        business.handle_workspace_activation(3).await.unwrap();
        assert!(
            niri.moves().is_empty(),
            "a window pinned to DP-2 must not follow DP-1"
        );

        // Its own output does move it.
        business.handle_workspace_activation(2).await.unwrap();
        assert_eq!(niri.moved_to_id(2), vec![1]);
    }

    #[tokio::test]
    async fn test_own_output_follow_keeps_windows_on_their_monitor() {
        let niri = two_monitors();
        niri.set_window_workspace(1, 2); // DP-2
        niri.set_window_workspace(2, 1); // DP-1
        let business = business_with_config(
            &niri,
            r#"
sticky-follow = "own-output"

[sticky.terminals]
app-id = "foot"
"#,
        );

        for id in [1, 2] {
            business
                .handle_window_opened_or_changed(id, Some("foot".into()), None, false)
                .await
                .unwrap();
        }
        assert_eq!(business.pin_of(1).await, vec!["DP-2".to_string()]);
        assert_eq!(business.pin_of(2).await, vec!["DP-1".to_string()]);

        business.handle_workspace_activation(2).await.unwrap();

        assert_eq!(
            niri.moved_to_id(2),
            vec![1],
            "only the window on DP-2 follows DP-2"
        );
    }

    #[tokio::test]
    async fn test_pinned_window_returns_to_its_output_when_it_drifted() {
        let niri = two_monitors();
        niri.set_window_workspace(1, 1); // currently on DP-1
        let business = business_with_config(
            &niri,
            r#"
[sticky.browser]
app-id = "foot"
output = "DP-2"
"#,
        );
        business
            .handle_window_opened_or_changed(1, Some("foot".into()), None, false)
            .await
            .unwrap();

        business.handle_workspace_activation(3).await.unwrap();

        assert_eq!(
            niri.moved_to_id(2),
            vec![1],
            "it goes back to the workspace active on DP-2"
        );
    }

    #[tokio::test]
    async fn test_pinned_window_follows_focus_while_its_output_is_disconnected() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        // Only DP-1 is connected.
        niri.set_workspaces(&[(1, "one", "DP-1")]);
        niri.set_workspace_active(1, true);
        niri.set_window_workspace(1, 1);
        let business = business_with_config(
            &niri,
            r#"
[sticky.browser]
app-id = "foot"
output = "DP-2"
"#,
        );
        business
            .handle_window_opened_or_changed(1, Some("foot".into()), None, false)
            .await
            .unwrap();

        business.handle_workspace_activation(1).await.unwrap();

        assert_eq!(
            niri.moved_to_id(1),
            vec![1],
            "an unplugged monitor must not make the window unreachable"
        );
    }

    #[tokio::test]
    async fn test_overlapping_output_rules_take_the_first_match() {
        let niri = two_monitors();
        niri.set_window_workspace(1, 1);
        niri.set_window_workspace(2, 2);
        let business = business_with_config(
            &niri,
            r#"
[sticky.left]
app-id = "foot"
output = "DP-1"

[sticky.right]
app-id = "foot"
output = "DP-2"
"#,
        );

        for id in [1, 2] {
            business
                .handle_window_opened_or_changed(
                    id,
                    Some("foot".into()),
                    Some("Terminal".into()),
                    false,
                )
                .await
                .unwrap();
        }

        // Both rules match both windows; sticky rules are evaluated in name
        // order, so "left" wins. Per-window pinning is what `sticky-follow =
        // "own-output"` (above) and distinct titles/apps are for.
        assert_eq!(business.pin_of(1).await, vec!["DP-1".to_string()]);
        assert_eq!(business.pin_of(2).await, vec!["DP-1".to_string()]);
    }

    /// Every entry point re-enters the state, config and compositor locks, and
    /// the stage workspace is created and released on the way; those tests run
    /// under a deadline so a lock taken twice fails instead of hanging.
    async fn scratchpad_business(niri: &Arc<FakeNiri>) -> BusinessLogic {
        business_with_config(niri, SCRATCHPAD_CONFIG)
    }

    #[tokio::test]
    async fn test_scratchpad_starts_the_command_when_nothing_matches() {
        let niri = with_layout();
        let business = scratchpad_business(&niri).await;

        let message = business.toggle_scratchpad(Some("term")).await.unwrap();

        assert!(message.starts_with("Starting term"), "{message}");
        assert_eq!(
            niri.window_actions(),
            vec!["spawn foot --app-id foot --title dropdown-terminal".to_string()]
        );
    }

    #[tokio::test]
    async fn test_scratchpad_hides_a_visible_window() {
        let niri = with_layout();
        let business = scratchpad_business(&niri).await;
        niri.set_windows(&[(1, "foot", "dropdown-terminal"), (2, "foot", "otra cosa")]);

        let message = business.toggle_scratchpad(Some("term")).await.unwrap();

        assert!(message.starts_with("Hidden term"), "{message}");
        assert!(parked(&business, 1).await);
        assert!(!parked(&business, 2).await, "only the matching window");
        assert!(
            !staged(&business, 1).await,
            "a scratchpad window is not a staged window: `stage restore` must not bring it back"
        );
        assert_eq!(
            niri.moved_to_id(3),
            vec![1],
            "the scratchpad area is the empty tail, not the stage"
        );
        assert!(niri.window_actions().is_empty(), "hiding styles nothing");
    }

    #[tokio::test]
    async fn test_a_scratchpad_parks_on_its_own_workspace() {
        let niri = with_layout();
        let business = business_with_config(
            &niri,
            r#"
stage-workspace = "parking"
scratchpad-workspace = "drop"

[scratchpad.term]
app-id = "foot"
title = "dropdown-terminal"
"#,
        );
        // A window parked on the stage, so that area is in use and stays, and a
        // window for the scratchpad to take.
        niri.set_workspaces(&[(1, "one", "DP-1"), (2, "parking", "DP-1"), (3, "", "DP-1")]);
        niri.set_windows(&[(1, "foot", "dropdown-terminal"), (2, "zen", "GitHub")]);
        niri.set_window_workspace(1, 1);
        niri.set_window_workspace(2, 1);
        business.stage_window(2).await.unwrap();

        let message = business.toggle_scratchpad(Some("term")).await.unwrap();

        assert!(message.starts_with("Hidden term"), "{message}");
        assert_eq!(
            niri.named_workspaces(),
            vec![
                (1, "one".to_string()),
                (2, "parking".to_string()),
                (3, "drop".to_string()),
            ],
            "the new area takes the empty tail, not the stage workspace"
        );
        assert_eq!(
            niri.moved_to_id(3),
            vec![1],
            "the window went to the scratchpad workspace"
        );

        // And it goes away again once the window is back.
        let message = business.toggle_scratchpad(Some("term")).await.unwrap();
        assert!(message.starts_with("Shown term"), "{message}");
        assert_eq!(
            niri.named_workspaces(),
            vec![(1, "one".to_string()), (2, "parking".to_string())],
            "the empty scratchpad workspace is released"
        );
    }

    #[tokio::test]
    async fn test_stage_restore_leaves_a_scratchpad_window_alone() {
        let niri = with_layout();
        let business = scratchpad_business(&niri).await;
        niri.set_windows(&[(1, "foot", "dropdown-terminal")]);

        business.toggle_scratchpad(Some("term")).await.unwrap();
        let _ = niri.moves();

        let restored = business.unstage_all_windows(1).await.unwrap();

        assert!(restored.succeeded.is_empty(), "{restored:?}");
        assert_eq!(
            business.list_staged_windows().await.unwrap(),
            Vec::<u64>::new(),
            "the dropdown terminal is not a staged window"
        );
        assert!(parked(&business, 1).await);
    }

    #[tokio::test]
    async fn test_parking_workspaces_are_kept_at_the_end() {
        let niri = with_layout();
        niri.set_workspaces(&[
            (1, "one", "DP-1"),
            (2, "drop", "DP-1"),
            (3, "parking", "DP-1"),
            (4, "", "DP-1"),
        ]);
        niri.set_window_workspace(1, 1);
        niri.set_window_workspace(2, 2); // the parked windows really are there
        niri.set_window_workspace(3, 3);
        let business = business_with_config(
            &niri,
            r#"
stage-workspace = "parking"
scratchpad-workspace = "drop"
"#,
        );

        // The user opened a window on the tail, so a workspace appeared below
        // the parking areas.
        niri.set_workspaces(&[
            (1, "one", "DP-1"),
            (2, "drop", "DP-1"),
            (3, "parking", "DP-1"),
            (5, "five", "DP-1"),
            (6, "", "DP-1"),
        ]);
        business.ensure_parking_order().await.unwrap();

        assert_eq!(
            niri.workspace_naming().join("; "),
            "movews 2 5; movews 3 6",
            "both areas move below the new workspace, in order"
        );

        // Looking again changes nothing: the layout is read from the
        // compositor, so after the moves it is already the wanted one. Acting on
        // a stale view is what made the daemon move them back and forth
        // forever.
        business.ensure_parking_order().await.unwrap();
        assert_eq!(
            niri.workspace_naming().join("; "),
            "movews 2 5; movews 3 6",
            "the second look must be satisfied"
        );
    }

    #[tokio::test]
    async fn test_scratchpads_survive_a_restart() {
        let niri = with_layout();
        let storage = Arc::new(MemoryStorage::default());
        {
            let business = business_with_storage(&niri, SCRATCHPAD_CONFIG, &storage);
            niri.set_windows(&[(1, "foot", "dropdown-terminal")]);
            business.toggle_scratchpad(Some("term")).await.unwrap();
        }

        let business = business_with_storage(&niri, SCRATCHPAD_CONFIG, &storage);
        business.reconcile().await.unwrap();

        assert!(parked(&business, 1).await);
        assert!(!staged(&business, 1).await);
    }

    #[tokio::test]
    async fn test_scratchpad_shows_a_parked_window_centred_and_sized() {
        let niri = with_layout();
        let business = scratchpad_business(&niri).await;
        niri.set_windows(&[(1, "foot", "dropdown-terminal")]);
        business.stage_window(1).await.unwrap();

        // Adopting a window parked on the stage moves it to the scratchpad.
        let message = business.toggle_scratchpad(Some("term")).await.unwrap();
        assert!(message.starts_with("Hidden term"), "{message}");
        assert!(parked(&business, 1).await);
        assert!(!staged(&business, 1).await, "the stage gave it up");

        let message = business.toggle_scratchpad(Some("term")).await.unwrap();

        assert!(message.starts_with("Shown term"), "{message}");
        assert!(!parked(&business, 1).await);
        assert!(!staged(&business, 1).await);
        assert_eq!(
            niri.window_actions(),
            vec![
                "float 1".to_string(),
                "resize 1 width=60% height=400px".to_string(),
                "focus 1".to_string(),
            ],
            "floated, sized and focused, in that order"
        );
        assert_eq!(
            niri.moves().last().unwrap(),
            &(1, 1),
            "back to the active workspace"
        );
    }

    #[tokio::test]
    async fn test_scratchpad_toggles_back_and_forth() {
        let niri = with_layout();
        let business = scratchpad_business(&niri).await;
        niri.set_windows(&[(1, "foot", "dropdown-terminal")]);

        assert!(
            business
                .toggle_scratchpad(Some("term"))
                .await
                .unwrap()
                .starts_with("Hidden")
        );
        assert!(
            business
                .toggle_scratchpad(Some("term"))
                .await
                .unwrap()
                .starts_with("Shown")
        );

        let hidden = business.toggle_scratchpad(Some("term")).await.unwrap();
        assert!(hidden.starts_with("Hidden"), "{hidden}");
        assert!(parked(&business, 1).await);
        assert!(!staged(&business, 1).await);
    }

    #[tokio::test]
    async fn test_scratchpad_remembers_the_size_and_position_it_had() {
        let niri = with_layout();
        let business = scratchpad_business(&niri).await;
        // The window is already floating with a size niri gave it; the config
        // asks for 60%, which must not win over what it actually had.
        niri.set_windows(&[(1, "foot", "dropdown-terminal")]);
        niri.set_window_floating(1, true);
        niri.set_window_geometry(1, (1100, 480), (730.0, 94.0));

        business.toggle_scratchpad(Some("term")).await.unwrap();
        let actions_after_hide = niri.window_actions();
        assert!(
            actions_after_hide.is_empty(),
            "hiding only parks it: {actions_after_hide:?}"
        );

        business.toggle_scratchpad(Some("term")).await.unwrap();

        assert_eq!(
            niri.window_actions(),
            vec![
                "float 1".to_string(),
                "resize 1 width=1100px height=480px".to_string(),
                "move 1 730 94".to_string(),
                "focus 1".to_string(),
            ],
            "it comes back exactly as it was, not with the configured 60%"
        );
    }

    #[tokio::test]
    async fn test_scratchpad_follows_focus_without_a_matcher() {
        let niri = with_layout();
        let business = business_with_config(
            &niri,
            r#"
stage-workspace = "stage"

[scratchpad.any]
float = true
"#,
        );
        niri.set_active_window(3);

        let hidden = business.toggle_scratchpad(Some("any")).await.unwrap();
        assert!(hidden.starts_with("Hidden any (3)"), "{hidden}");
        assert!(parked(&business, 3).await);

        // Something else has focus now: the toggle still brings back its window.
        niri.set_active_window(1);
        let shown = business.toggle_scratchpad(Some("any")).await.unwrap();

        assert!(shown.starts_with("Shown any (3)"), "{shown}");
        assert!(!parked(&business, 3).await);
        assert!(!parked(&business, 1).await, "the other window is untouched");
    }

    #[tokio::test]
    async fn test_scratchpad_without_a_name_toggles_the_focused_window() {
        let niri = with_layout();
        let business = business(&niri);
        niri.set_active_window(2);

        let hidden = business.toggle_scratchpad(None).await.unwrap();
        assert!(hidden.starts_with("Hidden focused (2)"), "{hidden}");

        let shown = business.toggle_scratchpad(None).await.unwrap();
        assert!(shown.starts_with("Shown focused (2)"), "{shown}");
        assert!(!parked(&business, 2).await);
        assert_eq!(
            niri.window_actions(),
            vec!["float 2".to_string(), "focus 2".to_string(),],
            "no geometry was configured or remembered, so nothing is resized"
        );
    }

    #[tokio::test]
    async fn test_scratchpad_without_a_spawn_command_reports_it() {
        let niri = with_layout();
        niri.set_windows(&[(9, "zen", "Browser")]);
        let business = business_with_config(
            &niri,
            r#"
[scratchpad.term]
app-id = "foot"
"#,
        );

        let error = business.toggle_scratchpad(Some("term")).await.unwrap_err();
        assert!(error.to_string().contains("no `spawn` command"), "{error}");
    }

    #[tokio::test]
    async fn test_unknown_scratchpad_names_the_configured_ones() {
        let niri = with_layout();
        let business = scratchpad_business(&niri).await;

        let error = business.toggle_scratchpad(Some("nope")).await.unwrap_err();
        assert!(error.to_string().contains("configured: term"), "{error}");
    }

    #[tokio::test]
    async fn test_handle_request_scratchpad() {
        let niri = with_layout();
        let business = scratchpad_business(&niri).await;

        match business
            .handle_request(Request::Scratchpad {
                name: Some("term".to_string()),
            })
            .await
        {
            Response::Success { message } => assert!(message.starts_with("Starting term")),
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_entry_points_do_not_deadlock() {
        let niri = with_layout();
        let business = business_with_config(&niri, SCRATCHPAD_CONFIG);

        with_deadline(async {
            business.add_sticky_window(1).await.unwrap();
            business.stage_window(1).await.unwrap();
            business.unstage_window(1, 1).await.unwrap();
            business.stage_all_windows().await.unwrap();
            business.unstage_all_windows(1).await.unwrap();
            business.toggle_active_window().await.unwrap();
            business
                .handle_request(Request::Stage { window_id: 1 })
                .await;
            business
                .handle_request(Request::Unstage { window_id: 1 })
                .await;
            business
                .handle_window_opened_or_changed(1, Some("foot".into()), None, false)
                .await
                .unwrap();
            business.handle_window_closed(1).await.unwrap();
            business.reconcile().await.unwrap();
            business.toggle_scratchpad(Some("term")).await.unwrap();
            business
                .handle_request(Request::Scratchpad {
                    name: Some("term".to_string()),
                })
                .await;
        })
        .await;
    }

    #[tokio::test]
    async fn test_reload_re_pins_tracked_windows() {
        let config_file = TempConfig::new();
        config_file.write(
            r#"
[sticky.browser]
app-id = "foot"
"#,
        );
        let niri = two_monitors();
        niri.set_window_workspace(1, 2); // DP-2
        let business = BusinessLogic::new(
            Config::from_toml("").unwrap(),
            config_file.path(),
            niri.clone(),
            storage(),
        );
        business.reload_from_disk().await.unwrap();
        business
            .handle_window_opened_or_changed(1, Some("foot".into()), None, false)
            .await
            .unwrap();
        assert!(
            business.pin_of(1).await.is_empty(),
            "follows focus by default"
        );

        // Adding an output pin only needs a reload.
        config_file.write(
            r#"
[sticky.browser]
app-id = "foot"
output = "DP-2"
"#,
        );
        let message = business.reload_from_disk().await.unwrap();

        assert!(message.contains("1 window(s) re-pinned"), "{message}");
        assert_eq!(business.pin_of(1).await, vec!["DP-2".to_string()]);
    }

    #[tokio::test]
    async fn test_floating_rule_re_evaluated_on_reload() {
        let config_file = TempConfig::new();
        config_file.write("\n");
        let niri = two_monitors();
        let business = BusinessLogic::new(
            Config::from_toml("").unwrap(),
            config_file.path(),
            niri.clone(),
            storage(),
        );
        business.stick(1, Vec::new()).await.unwrap();
        niri.set_window_floating(1, true);

        config_file.write(
            r#"
[sticky.pip]
floating = true
output = "DP-2"
"#,
        );
        business.reload_from_disk().await.unwrap();

        assert_eq!(
            business.pin_of(1).await,
            vec!["DP-2".to_string()],
            "the floating window picked up the new pin"
        );
    }

    #[tokio::test]
    async fn test_sticky_windows_follow_the_focused_workspace() {
        // The watcher forwards focused activations only, which this pins down.
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = business(&niri);
        business.add_sticky_window(1).await.unwrap();

        business.handle_workspace_activation(5).await.unwrap();

        assert_eq!(niri.moved_to_id(5), vec![1]);
    }

    #[tokio::test]
    async fn test_handle_workspace_activation_moves_sticky_windows() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = business(&niri);
        business.add_sticky_window(1).await.unwrap();
        business.add_sticky_window(2).await.unwrap();

        business.handle_workspace_activation(5).await.unwrap();

        let mut moved = niri.moved_to_id(5);
        moved.sort_unstable();
        assert_eq!(moved, vec![1, 2]);
    }

    #[tokio::test]
    async fn test_handle_workspace_activation_reports_niri_failures_gracefully() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = business(&niri);
        niri.fail_queries(true);

        // A failing compositor must not turn into a hard error for the watcher.
        business.handle_workspace_activation(5).await.unwrap();
    }

    #[tokio::test]
    async fn test_auto_sticky_matches_config_rules() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = business_with_config(
            &niri,
            r#"
[sticky.browser]
app-id = "firefox"
exclude-title = "Gmail"
"#,
        );

        business
            .handle_window_opened_or_changed(3, Some("firefox".into()), Some("Gmail".into()), false)
            .await
            .unwrap();
        assert!(!sticky(&business, 3).await, "excluded by title");

        business
            .handle_window_opened_or_changed(3, Some("firefox".into()), Some("Docs".into()), false)
            .await
            .unwrap();
        assert!(sticky(&business, 3).await);
    }

    #[tokio::test]
    async fn test_auto_stage_parks_matching_windows() {
        let niri = with_layout();
        let business = business_with_config(
            &niri,
            r#"
[stage.games]
app-id = "steam"
"#,
        );
        assert_eq!(
            business
                .config()
                .await
                .match_rule(&WindowFacts {
                    app_id: Some("steam"),
                    ..Default::default()
                })
                .map(|matched| matched.action),
            Some(WindowAction::Stage)
        );

        business
            .handle_window_opened_or_changed(4, Some("steam".into()), Some("Game".into()), false)
            .await
            .unwrap();

        assert!(staged(&business, 4).await);
        assert_eq!(niri.moved_to_id(2), vec![4]);
    }

    #[tokio::test]
    async fn test_auto_stage_fires_once_so_a_restore_wins() {
        let niri = with_layout();
        let business = business_with_config(
            &niri,
            r#"
[stage.games]
app-id = "steam"
"#,
        );

        business
            .handle_window_opened_or_changed(4, Some("steam".into()), None, false)
            .await
            .unwrap();
        business.unstage_window(4, 42).await.unwrap();

        // The move itself produces a WindowOpenedOrChanged event.
        business
            .handle_window_opened_or_changed(4, Some("steam".into()), None, false)
            .await
            .unwrap();

        assert!(!staged(&business, 4).await, "restore must win");
        assert_eq!(niri.moved_to_id(2), vec![4]);
    }

    #[tokio::test]
    async fn test_auto_stage_retries_after_a_failed_move() {
        let niri = with_layout();
        let business = business_with_config(
            &niri,
            r#"
[stage.games]
app-id = "steam"
"#,
        );
        niri.fail_move(4);

        assert!(
            business
                .handle_window_opened_or_changed(4, Some("steam".into()), None, false)
                .await
                .is_err()
        );
        assert!(!staged(&business, 4).await);

        // The compositor accepts the move again: a later event retries.
        let niri = with_layout();
        let business = business_with_config(
            &niri,
            r#"
[stage.games]
app-id = "steam"
"#,
        );
        business
            .handle_window_opened_or_changed(4, Some("steam".into()), None, false)
            .await
            .unwrap();
        assert!(staged(&business, 4).await);
    }

    #[tokio::test]
    async fn test_sticky_and_stage_rules_coexist() {
        let niri = with_layout();
        let business = business_with_config(
            &niri,
            r#"
[sticky.terminals]
app-id = "foot"

[stage.games]
app-id = "steam"
"#,
        );

        business
            .handle_window_opened_or_changed(1, Some("foot".into()), Some("Terminal".into()), false)
            .await
            .unwrap();
        business
            .handle_window_opened_or_changed(4, Some("steam".into()), Some("Game".into()), false)
            .await
            .unwrap();

        assert!(sticky(&business, 1).await);
        assert!(!staged(&business, 1).await);
        assert!(staged(&business, 4).await);
        assert!(!sticky(&business, 4).await);
    }

    #[tokio::test]
    async fn test_reload_applies_new_rules_without_a_restart() {
        let config_file = TempConfig::new();
        config_file.write("\n");
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = BusinessLogic::new(
            Config::from_toml("").unwrap(),
            config_file.path(),
            niri.clone(),
            storage(),
        );

        // No rule yet: the window is left alone.
        business
            .handle_window_opened_or_changed(3, Some("firefox".into()), Some("Inbox".into()), false)
            .await
            .unwrap();
        assert!(!sticky(&business, 3).await);

        config_file.write(
            r#"
[sticky.browser]
app-id = "firefox"
"#,
        );
        business.reload_from_disk().await.unwrap();

        business
            .handle_window_opened_or_changed(3, Some("firefox".into()), Some("Inbox".into()), false)
            .await
            .unwrap();
        assert!(sticky(&business, 3).await, "the new rule applies");
    }

    #[tokio::test]
    async fn test_reload_keeps_the_running_config_when_the_file_is_broken() {
        let config_file = TempConfig::new();
        config_file.write(
            r#"
[sticky.browser]
app-id = "firefox"
"#,
        );
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = BusinessLogic::new(
            Config::from_toml("").unwrap(),
            config_file.path(),
            niri.clone(),
            storage(),
        );
        business.reload_from_disk().await.unwrap();

        config_file.write("[sticky.broken]\napp-id = \"[\"\n");
        let error = business.reload_from_disk().await.unwrap_err();
        assert!(
            format!("{error:#}").contains("Keeping the running configuration"),
            "{error:#}"
        );

        // The previous rules still work.
        business
            .handle_window_opened_or_changed(3, Some("firefox".into()), None, false)
            .await
            .unwrap();
        assert!(sticky(&business, 3).await);
    }

    #[tokio::test]
    async fn test_reload_reports_the_number_of_rules() {
        let config_file = TempConfig::new();
        config_file.write(
            r#"
[sticky.a]
app-id = "a"

[stage.b]
app-id = "b"
"#,
        );
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = BusinessLogic::new(
            Config::from_toml("").unwrap(),
            config_file.path(),
            niri.clone(),
            storage(),
        );

        let message = business.reload_from_disk().await.unwrap();
        assert_eq!(
            message,
            "Reloaded configuration: 2 rule(s), 0 window(s) re-pinned"
        );
    }

    #[tokio::test]
    async fn test_floating_rule_only_sticks_floating_windows() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = business_with_config(
            &niri,
            r#"
[sticky.pip]
app-id = "mpv"
floating = true
"#,
        );

        business
            .handle_window_opened_or_changed(4, Some("mpv".into()), Some("video".into()), false)
            .await
            .unwrap();
        assert!(!sticky(&business, 4).await, "tiled window must not match");

        business
            .handle_window_opened_or_changed(4, Some("mpv".into()), Some("video".into()), true)
            .await
            .unwrap();
        assert!(sticky(&business, 4).await);
    }

    #[tokio::test]
    async fn test_auto_sticky_empty_config_does_not_sticky() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = business(&niri);

        business
            .handle_window_opened_or_changed(1, Some("foot".into()), None, false)
            .await
            .unwrap();

        assert!(!sticky(&business, 1).await);
    }

    #[tokio::test]
    async fn test_handle_window_closed_clears_both_sets() {
        let niri = with_layout();
        let business = business(&niri);
        business.add_sticky_window(1).await.unwrap();
        business.stage_window(1).await.unwrap();

        business.handle_window_closed(1).await.unwrap();

        assert!(!sticky(&business, 1).await);
        assert!(!staged(&business, 1).await);
    }

    #[tokio::test]
    async fn test_handle_request_all_variants_return_valid_response() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = business(&niri);
        let variants = [
            Request::Add { window_id: 1 },
            Request::Remove { window_id: 1 },
            Request::List,
            Request::ToggleActive,
            Request::ToggleAppid {
                appid: "nonexistent".into(),
            },
            Request::ToggleTitle {
                title: "nonexistent".into(),
            },
            Request::StageList,
            Request::Stage { window_id: 1 },
            Request::Unstage { window_id: 1 },
            Request::StageToggleActive,
            Request::StageToggleAppid {
                appid: "nonexistent".into(),
            },
            Request::StageToggleTitle {
                title: "nonexistent".into(),
            },
            Request::StageAll,
            Request::UnstageAll,
            Request::Windows,
        ];
        for variant in variants {
            let response = business.handle_request(variant.clone()).await;
            assert!(
                matches!(
                    response,
                    Response::Success { .. } | Response::Error { .. } | Response::Data { .. }
                ),
                "Expected a valid Response variant, got {response:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_handle_request_list_returns_json_ids() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = business(&niri);
        business.add_sticky_window(1).await.unwrap();

        assert_eq!(
            business.handle_request(Request::List).await,
            Response::data("[1]")
        );
        assert_eq!(
            business.handle_request(Request::StageList).await,
            Response::data("[]")
        );
    }

    #[tokio::test]
    async fn test_handle_request_windows_serializes_the_window_list() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = business(&niri);

        match business.handle_request(Request::Windows).await {
            Response::Data { data } => {
                let windows: Vec<WindowInfo> = serde_json::from_str(&data).unwrap();
                assert_eq!(windows.len(), WINDOWS.len());
                assert_eq!(windows[0].id, 1);
            }
            other => panic!("expected data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_handle_request_reports_compositor_failures() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = business(&niri);
        niri.fail_queries(true);

        for request in [
            Request::Add { window_id: 1 },
            Request::ToggleActive,
            Request::Stage { window_id: 1 },
        ] {
            match business.handle_request(request.clone()).await {
                Response::Error { message } => assert!(!message.is_empty(), "{request:?}"),
                other => panic!("expected an error for {request:?}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn test_state_is_persisted_after_a_mutation_only() {
        let niri = with_layout();
        let storage = storage();
        let business = business_with_storage(&niri, "", &storage);

        // A read-only request writes nothing.
        business.handle_request(Request::List).await;
        assert_eq!(storage.saves(), 0);

        business.handle_request(Request::Add { window_id: 1 }).await;
        assert_eq!(storage.saves(), 1);
        assert_eq!(
            storage.snapshot().unwrap().sticky,
            vec![StoredSticky::Id(1)]
        );

        // Repeating the same mutation changes nothing, so it is not rewritten.
        business.handle_request(Request::Add { window_id: 1 }).await;
        assert_eq!(storage.saves(), 1);

        // A real change is written again.
        business
            .handle_request(Request::Stage { window_id: 1 })
            .await;
        assert_eq!(storage.saves(), 2);
        let stored = storage.snapshot().unwrap();
        assert!(stored.sticky.is_empty());
        assert_eq!(stored.staged.len(), 1);
        assert!(stored.staged[0].was_sticky);
    }

    #[tokio::test]
    async fn test_handle_window_closed_persists_the_removal() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let storage = storage();
        let business = business_with_storage(&niri, "", &storage);
        business.handle_request(Request::Add { window_id: 1 }).await;

        business.handle_window_closed(1).await.unwrap();

        assert!(storage.snapshot().unwrap().sticky.is_empty());
    }

    #[tokio::test]
    async fn test_persist_failures_do_not_break_window_management() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let storage = storage();
        storage.fail(true);
        let business = business_with_storage(&niri, "", &storage);

        // The command still succeeds: a read-only filesystem must not stop
        // nsticky from managing windows.
        assert_eq!(
            business.handle_request(Request::Add { window_id: 1 }).await,
            Response::success("Added")
        );
        assert!(sticky(&business, 1).await);
    }

    #[tokio::test]
    async fn test_reconcile_restores_state_and_drops_dead_windows() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let storage = Arc::new(MemoryStorage::with_state(StoredState::new(
            vec![StoredSticky::Id(1), StoredSticky::Id(999)],
            vec![StoredStaged {
                id: 2,
                was_sticky: true,
                outputs: Vec::new(),
                size: None,
                position: None,
            }],
        )));
        let business = business_with_storage(&niri, "", &storage);

        business.reconcile().await.unwrap();

        assert!(sticky(&business, 1).await);
        assert!(!sticky(&business, 999).await, "window 999 is gone");
        assert!(staged(&business, 2).await);
        assert_eq!(
            storage.snapshot().unwrap().sticky,
            vec![StoredSticky::Id(1)]
        );
    }

    #[tokio::test]
    async fn test_reconcile_adopts_windows_sitting_on_the_stage_workspace() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        niri.set_workspaces(&[(10, "main", "DP-1"), (20, "stage", "DP-1")]);
        niri.set_window_workspace(3, 20);
        let storage = storage();
        let business = business_with_storage(&niri, "", &storage);

        business.reconcile().await.unwrap();

        assert!(staged(&business, 3).await, "adopted from the workspace");
        assert!(!staged(&business, 1).await);
        assert!(!sticky(&business, 3).await);
    }

    #[tokio::test]
    async fn test_reconcile_uses_the_configured_stage_workspace() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        niri.set_workspaces(&[(10, "main", "DP-1"), (20, "parking", "DP-1")]);
        niri.set_window_workspace(2, 20);
        let storage = storage();
        let business = business_with_storage(&niri, "stage-workspace = \"parking\"\n", &storage);

        business.reconcile().await.unwrap();

        assert!(staged(&business, 2).await);
    }

    #[tokio::test]
    async fn test_reconcile_prefers_staged_over_sticky() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        niri.set_workspaces(&[(10, "main", "DP-1"), (20, "stage", "DP-1")]);
        niri.set_window_workspace(1, 20);
        let storage = Arc::new(MemoryStorage::with_state(StoredState::new(
            vec![StoredSticky::Id(1)],
            Vec::new(),
        )));
        let business = business_with_storage(&niri, "", &storage);

        business.reconcile().await.unwrap();

        assert!(staged(&business, 1).await);
        assert!(!sticky(&business, 1).await, "never both at once");
    }

    #[tokio::test]
    async fn test_reconcile_drops_windows_that_left_the_stage_workspace() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        niri.set_workspaces(&[(10, "main", "DP-1"), (20, "stage", "DP-1")]);
        // Window 3 is back on the main workspace, but the store still lists it.
        niri.set_window_workspace(3, 10);
        let storage = Arc::new(MemoryStorage::with_state(StoredState::new(
            Vec::new(),
            vec![StoredStaged {
                id: 3,
                was_sticky: true,
                outputs: Vec::new(),
                size: None,
                position: None,
            }],
        )));
        let business = business_with_storage(&niri, "", &storage);

        business.reconcile().await.unwrap();

        assert!(
            !staged(&business, 3).await,
            "the compositor says it is not parked"
        );
        assert!(
            storage.snapshot().unwrap().staged.is_empty(),
            "the stale entry is dropped from storage too"
        );
    }

    #[tokio::test]
    async fn test_reconcile_recovers_was_sticky_from_the_store() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        niri.set_workspaces(&[(10, "main", "DP-1"), (20, "stage", "DP-1")]);
        niri.set_window_workspace(1, 20);
        let storage = Arc::new(MemoryStorage::with_state(StoredState::new(
            Vec::new(),
            vec![StoredStaged {
                id: 1,
                was_sticky: true,
                outputs: Vec::new(),
                size: None,
                position: None,
            }],
        )));
        let business = business_with_storage(&niri, "", &storage);

        business.reconcile().await.unwrap();
        business.unstage_window(1, 10).await.unwrap();

        assert!(sticky(&business, 1).await, "it was sticky before staging");
    }

    #[tokio::test]
    async fn test_reconcile_without_a_store_still_adopts_the_stage_workspace() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        niri.set_workspaces(&[(10, "main", "DP-1"), (20, "stage", "DP-1")]);
        niri.set_window_workspace(4, 20);
        let storage = storage();
        let business = business_with_storage(&niri, "", &storage);

        business.reconcile().await.unwrap();

        assert!(staged(&business, 4).await);
        assert!(
            storage.snapshot().unwrap().staged.iter().any(|s| s.id == 4),
            "the adopted window is persisted too"
        );
    }

    #[tokio::test]
    async fn test_reconcile_keeps_stored_state_when_niri_is_unavailable() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let storage = Arc::new(MemoryStorage::with_state(StoredState::new(
            vec![StoredSticky::Id(1), StoredSticky::Id(2)],
            vec![StoredStaged {
                id: 3,
                was_sticky: true,
                outputs: Vec::new(),
                size: None,
                position: None,
            }],
        )));
        let business = business_with_storage(&niri, "", &storage);
        niri.fail_queries(true);

        business.reconcile().await.unwrap();

        assert!(sticky(&business, 1).await, "state must not be wiped");
        assert!(sticky(&business, 2).await);
        assert!(staged(&business, 3).await);
    }

    #[tokio::test]
    async fn test_reconcile_ignores_a_broken_store() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        niri.set_workspaces(&[(10, "main", "DP-1"), (20, "stage", "DP-1")]);
        niri.set_window_workspace(1, 20);
        let storage = storage();
        storage.fail(true);
        let business = business_with_storage(&niri, "", &storage);

        business.reconcile().await.unwrap();

        // The compositor is still the source of truth.
        assert!(staged(&business, 1).await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_commands_do_not_interleave() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = business(&niri);

        let (first, second) = tokio::join!(
            business.handle_request(Request::Add { window_id: 1 }),
            business.handle_request(Request::Add { window_id: 2 }),
        );

        assert_eq!(first, Response::success("Added"));
        assert_eq!(second, Response::success("Added"));
        assert_eq!(
            niri.max_in_flight_calls(),
            1,
            "operations must not overlap: each reads the compositor, changes state and moves windows"
        );
    }

    #[test]
    fn test_reply_maps_results_to_responses() {
        let ok = reply(Ok::<u8, anyhow::Error>(2), |count| {
            format!("Toggled {count} window(s)")
        });
        assert_eq!(ok, Response::success("Toggled 2 window(s)"));

        let outcome = BatchOutcome {
            succeeded: vec![1, 2],
            failed: vec![5],
        };
        assert_eq!(
            reply(Ok::<_, anyhow::Error>(outcome), |outcome| outcome
                .message("Staged", "windows")),
            Response::success("Staged 2 windows, 1 failed: 5")
        );

        let error = reply(Err::<u8, _>(anyhow!("No window found")), |count| {
            format!("{count}")
        });
        assert_eq!(error, Response::error("No window found"));
    }

    #[test]
    fn test_reply_json_serializes_payloads() {
        let data = reply_json(Ok::<Vec<u64>, anyhow::Error>(vec![7, 9]));
        assert_eq!(data, Response::data("[7,9]"));

        let empty = reply_json(Ok::<Vec<u64>, anyhow::Error>(Vec::new()));
        assert_eq!(empty, Response::data("[]"));
    }

    #[test]
    fn test_reply_json_turns_failures_into_errors() {
        let failed = reply_json(Err::<Vec<u64>, anyhow::Error>(anyhow!("No window found")));
        assert_eq!(failed, Response::error("No window found"));

        // A payload that cannot be encoded. Nothing in the daemon produces one,
        // but a request must still be answered instead of panicking.
        struct Unserializable;

        impl serde::Serialize for Unserializable {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("not encodable"))
            }
        }

        match reply_json(Ok::<Unserializable, anyhow::Error>(Unserializable)) {
            Response::Error { message } => assert!(
                message.starts_with("Failed to serialize response: "),
                "{message}"
            ),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    thread_local! {
        /// Lines captured by [`with_logs`] on this thread. `None` outside a
        /// capture, so other tests' events are dropped instead of collected.
        static LOG_LINES: std::cell::RefCell<Option<Vec<u8>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Writes tracing output into the current thread's buffer.
    struct LogSink;

    impl std::io::Write for LogSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            LOG_LINES.with(|lines| {
                if let Some(lines) = lines.borrow_mut().as_mut() {
                    lines.extend_from_slice(bytes);
                }
            });
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for LogSink {
        type Writer = LogSink;

        fn make_writer(&self) -> Self::Writer {
            LogSink
        }
    }

    /// Install the capturing subscriber once for the whole test binary.
    ///
    /// A subscriber per capture would have to be set and dropped around every
    /// one of them, and `tracing` rebuilds the interest cache of every call
    /// site on both, which can drop an event that lands during the rebuild.
    /// Installing it once keeps every call site enabled for the whole run; the
    /// per-test separation comes from the thread-local buffer.
    fn capture_logs() {
        static INSTALL: std::sync::Once = std::sync::Once::new();

        INSTALL.call_once(|| {
            let _ = tracing_subscriber::fmt()
                .with_writer(LogSink)
                .with_max_level(tracing_subscriber::filter::LevelFilter::DEBUG)
                .with_ansi(false)
                .without_time()
                .try_init();
        });
    }

    /// The value a future produced and the log lines it wrote while producing
    /// it, so a test can check that a best-effort failure reached the log
    /// instead of being swallowed.
    async fn with_logs<T>(future: impl Future<Output = T>) -> (T, Vec<String>) {
        capture_logs();
        LOG_LINES.with(|lines| *lines.borrow_mut() = Some(Vec::new()));

        let value = future.await;

        let bytes = LOG_LINES
            .with(|lines| lines.borrow_mut().take())
            .unwrap_or_default();
        let logs = String::from_utf8(bytes).unwrap();
        (value, logs.lines().map(str::to_string).collect())
    }

    /// Whether the log mentions this failure. `message` comes from the fake
    /// compositor, so this checks the error reached the log, not the wording.
    fn reported(logs: &[String], message: &str) -> bool {
        logs.iter().any(|line| line.contains(message))
    }

    #[tokio::test]
    async fn test_scratchpad_name_without_configuration_reports_it() {
        let niri = with_layout();
        let business = business(&niri);

        match business
            .handle_request(Request::Scratchpad {
                name: Some("term".to_string()),
            })
            .await
        {
            Response::Error { message } => {
                assert_eq!(message, "No [scratchpad.term] is configured")
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_scratchpad_hide_reports_a_refused_move_and_keeps_its_state() {
        let niri = with_layout();
        let storage = storage();
        let business = business_with_storage(&niri, SCRATCHPAD_CONFIG, &storage);
        niri.set_windows(&[(1, "foot", "dropdown-terminal")]);
        business.handle_request(Request::Add { window_id: 1 }).await;
        let before = storage.snapshot().unwrap();
        assert_eq!(before.sticky.len(), 1);

        niri.fail_move(1);
        let response = business
            .handle_request(Request::Scratchpad {
                name: Some("term".to_string()),
            })
            .await;

        match response {
            Response::Error { message } => {
                assert!(message.contains("refused to move"), "{message}")
            }
            other => panic!("expected an error, got {other:?}"),
        }
        assert!(!parked(&business, 1).await, "it stayed where it was");
        assert!(sticky(&business, 1).await, "and it is still sticky");
        let after = storage.snapshot().unwrap();
        assert_eq!(after.sticky, before.sticky, "the store still has it sticky");
        assert!(
            after.scratchpads.is_empty(),
            "no pad was recorded: {after:?}"
        );
    }

    #[tokio::test]
    async fn test_scratchpad_show_reports_a_refused_move_and_stays_parked() {
        let niri = with_layout();
        let storage = storage();
        let business = business_with_storage(&niri, SCRATCHPAD_CONFIG, &storage);
        niri.set_windows(&[(1, "foot", "dropdown-terminal")]);
        business
            .handle_request(Request::Scratchpad {
                name: Some("term".to_string()),
            })
            .await;
        let parked_state = storage.snapshot().unwrap();
        assert_eq!(parked_state.scratchpads.len(), 1);

        niri.fail_move(1);
        let response = business
            .handle_request(Request::Scratchpad {
                name: Some("term".to_string()),
            })
            .await;

        match response {
            Response::Error { message } => {
                assert!(message.contains("refused to move"), "{message}")
            }
            other => panic!("expected an error, got {other:?}"),
        }
        assert!(parked(&business, 1).await, "it stays parked");
        assert_eq!(
            storage.snapshot().unwrap(),
            parked_state,
            "and so does the stored state"
        );
    }

    #[tokio::test]
    async fn test_scratchpad_puts_a_sticky_window_back_on_the_sticky_list() {
        let niri = with_layout();
        let business = scratchpad_business(&niri).await;
        niri.set_windows(&[(1, "foot", "dropdown-terminal")]);
        business.add_sticky_window(1).await.unwrap();

        business.toggle_scratchpad(Some("term")).await.unwrap();
        assert!(!sticky(&business, 1).await, "not sticky while it is parked");

        let message = business.toggle_scratchpad(Some("term")).await.unwrap();

        assert!(message.starts_with("Shown term"), "{message}");
        assert!(sticky(&business, 1).await, "sticky again, as it was");
    }

    #[tokio::test]
    async fn test_scratchpad_reports_that_it_could_not_style_the_window() {
        let niri = with_layout();
        let business = scratchpad_business(&niri).await;
        niri.set_windows(&[(1, "foot", "dropdown-terminal")]);
        niri.set_window_floating(1, true);
        niri.set_window_geometry(1, (1100, 480), (730.0, 94.0));
        business.toggle_scratchpad(Some("term")).await.unwrap();

        niri.fail_next(Call::FloatWindow);
        niri.fail_next(Call::ResizeWindow);
        niri.fail_next(Call::MoveFloatingWindow);
        let (result, logs) = with_logs(business.toggle_scratchpad(Some("term"))).await;

        assert!(
            result.unwrap().starts_with("Shown term"),
            "the window comes back even when niri refuses to style it"
        );
        assert!(!parked(&business, 1).await);
        for failure in [
            "injected failure for float_window",
            "injected failure for resize_window",
            "injected failure for move_floating_window",
        ] {
            assert!(reported(&logs, failure), "{failure}: {logs:?}");
        }
    }

    #[tokio::test]
    async fn test_scratchpad_corrects_the_gap_niri_leaves_when_it_places_the_window() {
        let niri = with_layout();
        let business = scratchpad_business(&niri).await;
        niri.set_windows(&[(1, "foot", "dropdown-terminal")]);
        niri.set_window_floating(1, true);
        niri.set_window_geometry(1, (1100, 480), (730.0, 94.0));
        business.toggle_scratchpad(Some("term")).await.unwrap();

        // niri placed the window 10px lower than asked, as it does when a bar
        // takes part of the working area: the second move takes that back out.
        niri.set_window_geometry(1, (1100, 480), (730.0, 104.0));
        business.toggle_scratchpad(Some("term")).await.unwrap();

        assert_eq!(
            niri.window_actions(),
            vec![
                "float 1".to_string(),
                "resize 1 width=1100px height=480px".to_string(),
                "move 1 730 94".to_string(),
                "move 1 730 84".to_string(),
                "focus 1".to_string(),
            ],
            "the second move takes back out the offset niri left"
        );

        // When niri refuses that correction too, the caller is still answered.
        business.toggle_scratchpad(Some("term")).await.unwrap();
        niri.set_window_geometry(1, (1100, 480), (730.0, 114.0));
        niri.fail_nth(Call::MoveFloatingWindow, 2);
        let (result, logs) = with_logs(business.toggle_scratchpad(Some("term"))).await;

        assert!(result.is_ok(), "{result:?}");
        assert!(
            reported(&logs, "injected failure for move_floating_window"),
            "{logs:?}"
        );
    }

    #[tokio::test]
    async fn test_scratchpad_skips_the_correction_when_niri_reports_no_position() {
        let niri = with_layout();
        let business = scratchpad_business(&niri).await;
        niri.set_windows(&[(1, "foot", "dropdown-terminal")]);
        niri.set_window_floating(1, true);
        niri.set_window_geometry(1, (1100, 480), (730.0, 94.0));
        business.toggle_scratchpad(Some("term")).await.unwrap();

        // The window comes back tiled, so niri reports no position for it.
        niri.set_windows(&[(1, "foot", "dropdown-terminal")]);
        niri.set_window_workspace(1, 1);
        business.toggle_scratchpad(Some("term")).await.unwrap();

        assert_eq!(
            niri.window_actions(),
            vec![
                "float 1".to_string(),
                "resize 1 width=1100px height=480px".to_string(),
                "move 1 730 94".to_string(),
                "focus 1".to_string(),
            ],
            "the remembered position is asked for, but there is nothing to correct"
        );
    }

    #[tokio::test]
    async fn test_scratchpad_reports_a_refused_spawn_or_focus() {
        let niri = with_layout();
        let business = scratchpad_business(&niri).await;

        niri.fail_next(Call::Spawn);
        let error = business.toggle_scratchpad(Some("term")).await.unwrap_err();
        assert!(
            error.to_string().contains("injected failure for spawn"),
            "{error}"
        );

        niri.set_windows(&[(1, "foot", "dropdown-terminal")]);
        business.toggle_scratchpad(Some("term")).await.unwrap();
        niri.fail_next(Call::FocusWindow);
        let error = business.toggle_scratchpad(Some("term")).await.unwrap_err();

        assert!(
            error
                .to_string()
                .contains("injected failure for focus_window"),
            "{error}"
        );
        assert!(
            !parked(&business, 1).await,
            "the window is back on screen even though it could not be focused"
        );
    }

    #[tokio::test]
    async fn test_staging_reports_an_output_with_no_empty_workspace() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        // The only workspace on the output has windows on it: niri keeps no
        // empty tail to park on.
        niri.set_workspaces(&[(1, "one", "DP-1")]);
        niri.set_workspace_active(1, true);
        for id in [1, 2] {
            niri.set_window_workspace(id, 1);
        }
        let business = business(&niri);
        business.add_sticky_window(1).await.unwrap();

        let error = business.stage_window(1).await.unwrap_err();

        assert!(
            error
                .to_string()
                .contains("No empty workspace on DP-1 to turn into \"stage\""),
            "{error}"
        );
        assert!(!staged(&business, 1).await, "nothing was parked");
        assert!(sticky(&business, 1).await, "and it stays sticky");
        assert!(niri.moves().is_empty());
        assert!(niri.workspace_naming().is_empty());
    }

    #[tokio::test]
    async fn test_staging_reports_a_workspace_it_cannot_name() {
        let niri = with_layout();
        niri.set_workspaces(&[(1, "one", "DP-1"), (2, "", "DP-1")]);
        let business = business_with_config(&niri, "stage-workspace = \"parking\"\n");
        niri.fail_next(Call::NameWorkspace);

        let error = business.stage_window(1).await.unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Failed to name workspace 2 as \"parking\""),
            "{error}"
        );
        assert_eq!(
            business.list_staged_windows().await.unwrap(),
            Vec::<u64>::new()
        );
        assert!(
            niri.moves().is_empty(),
            "the window was not moved onto a workspace that has no name"
        );
    }

    #[tokio::test]
    async fn test_staging_survives_a_window_list_that_cannot_be_read() {
        let niri = with_layout();
        let storage = storage();
        let business = business_with_storage(&niri, "", &storage);
        niri.set_window_floating(1, true);
        niri.set_window_geometry(1, (800, 600), (10.0, 20.0));
        business.handle_request(Request::Add { window_id: 1 }).await;

        // The second window-list call is the geometry lookup, which then fails;
        // the window is still parked, without a remembered geometry.
        niri.fail_nth(Call::Windows, 2);
        let response = business
            .handle_request(Request::Stage { window_id: 1 })
            .await;

        assert!(matches!(response, Response::Success { .. }), "{response:?}");
        let stored = storage.snapshot().unwrap();
        assert_eq!(stored.staged.len(), 1, "{stored:?}");
        assert_eq!(stored.staged[0].size, None, "no geometry could be read");

        // With the compositor answering, the same window is parked with it.
        business
            .handle_request(Request::Unstage { window_id: 1 })
            .await;
        business
            .handle_request(Request::Stage { window_id: 1 })
            .await;

        assert_eq!(storage.snapshot().unwrap().staged[0].size, Some((800, 600)));
    }

    #[tokio::test]
    async fn test_releasing_the_stage_reports_a_refused_unname() {
        let niri = with_layout();
        let storage = storage();
        let business = business_with_storage(&niri, "", &storage);
        business
            .handle_request(Request::Stage { window_id: 1 })
            .await;

        niri.fail_next(Call::UnnameWorkspace);
        let (response, logs) =
            with_logs(business.handle_request(Request::Unstage { window_id: 1 })).await;

        assert!(matches!(response, Response::Success { .. }), "{response:?}");
        assert!(
            reported(&logs, "injected failure for unname_workspace"),
            "{logs:?}"
        );
        assert!(
            storage.snapshot().unwrap().staged.is_empty(),
            "the window is not staged any more"
        );
    }

    #[tokio::test]
    async fn test_releasing_the_stage_is_skipped_when_the_layout_is_unreadable() {
        let niri = with_layout();
        let business = business(&niri);
        business.stage_window(1).await.unwrap();

        niri.fail_next(Call::Workspaces);
        let (result, logs) = with_logs(business.unstage_window(1, 1)).await;

        assert!(result.is_ok(), "{result:?}");
        assert!(
            reported(&logs, "injected failure for workspaces"),
            "{logs:?}"
        );
        assert_eq!(
            business.list_staged_windows().await.unwrap(),
            Vec::<u64>::new()
        );
    }

    #[tokio::test]
    async fn test_reordering_the_parking_areas_reports_a_refused_move() {
        let niri = with_layout();
        niri.set_workspaces(&[
            (1, "one", "DP-1"),
            (2, "drop", "DP-1"),
            (3, "stage", "DP-1"),
            (5, "five", "DP-1"),
            (6, "", "DP-1"),
        ]);
        niri.set_window_workspace(1, 1);
        niri.set_window_workspace(2, 2);
        niri.set_window_workspace(3, 3);
        let business = business_with_config(&niri, "scratchpad-workspace = \"drop\"\n");

        niri.fail_next(Call::MoveWorkspaceToIndex);
        let (result, logs) = with_logs(business.ensure_parking_order()).await;

        assert!(result.is_ok(), "{result:?}");
        assert!(
            reported(&logs, "injected failure for move_workspace_to_index"),
            "{logs:?}"
        );
        assert_eq!(
            niri.workspace_naming().join("; "),
            "movews 3 6",
            "one refused move does not stop the other area from being ordered"
        );
    }

    #[tokio::test]
    async fn test_reconcile_restores_scratchpad_slots_from_the_store() {
        let niri = with_layout();
        let storage = Arc::new(MemoryStorage::default());
        {
            let business = business_with_storage(&niri, SCRATCHPAD_CONFIG, &storage);
            niri.set_windows(&[(1, "foot", "dropdown-terminal")]);
            business.toggle_scratchpad(Some("term")).await.unwrap();
        }

        // The compositor is down when the daemon comes back, so the file is all
        // there is to go on.
        niri.fail_queries(true);
        let business = business_with_storage(&niri, SCRATCHPAD_CONFIG, &storage);
        business.reconcile().await.unwrap();
        niri.fail_queries(false);

        let message = business.toggle_scratchpad(Some("term")).await.unwrap();

        assert_eq!(message, "Shown term (1)", "the parked window comes back");
        assert!(
            !niri
                .window_actions()
                .iter()
                .any(|action| action.starts_with("spawn")),
            "and its command is not started again: {:?}",
            niri.window_actions()
        );
    }

    #[tokio::test]
    async fn test_reconcile_without_the_compositor_or_a_store_still_answers() {
        let niri = FakeNiri::with_windows(WINDOWS);
        niri.fail_queries(true);
        let business = business(&Arc::new(niri));

        business.reconcile().await.unwrap();

        assert_eq!(
            business.list_staged_windows().await.unwrap(),
            Vec::<u64>::new()
        );
    }

    #[tokio::test]
    async fn test_reconcile_keeps_stored_staging_when_windows_report_no_workspace() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        niri.set_workspaces(&[(10, "main", "DP-1"), (20, "stage", "DP-1")]);
        // No window says which workspace it is on, so the stage workspace
        // cannot be matched against them.
        let storage = Arc::new(MemoryStorage::with_state(StoredState::new(
            Vec::new(),
            vec![StoredStaged {
                id: 1,
                was_sticky: true,
                outputs: Vec::new(),
                size: None,
                position: None,
            }],
        )));
        let business = business_with_storage(&niri, "", &storage);

        business.reconcile().await.unwrap();

        assert_eq!(business.list_staged_windows().await.unwrap(), vec![1]);
    }

    #[tokio::test]
    async fn test_reconcile_falls_back_to_the_store_when_the_layout_is_unreadable() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let storage = Arc::new(MemoryStorage::with_state(StoredState::new(
            Vec::new(),
            vec![StoredStaged {
                id: 3,
                was_sticky: false,
                outputs: Vec::new(),
                size: None,
                position: None,
            }],
        )));
        let business = business_with_storage(&niri, "", &storage);
        niri.fail_next(Call::Workspaces);

        let (result, logs) = with_logs(business.reconcile()).await;

        assert!(result.is_ok(), "{result:?}");
        assert!(
            reported(&logs, "injected failure for workspaces"),
            "{logs:?}"
        );
        assert_eq!(business.list_staged_windows().await.unwrap(), vec![3]);
    }

    #[tokio::test]
    async fn test_reload_re_pins_windows_and_leaves_the_others_alone() {
        let config_file = TempConfig::new();
        config_file.write(
            r#"
sticky-follow = "own-output"

[sticky.terminals]
app-id = "foot"
"#,
        );
        let niri = two_monitors();
        niri.set_window_workspace(1, 2); // DP-2
        niri.set_window_workspace(2, 1); // DP-1
        let business = BusinessLogic::new(
            Config::from_toml("").unwrap(),
            config_file.path(),
            niri.clone(),
            storage(),
        );
        for id in [1, 2] {
            business.add_sticky_window(id).await.unwrap();
        }
        assert_eq!(
            business.pin_of(1).await,
            Vec::<String>::new(),
            "no rule yet"
        );

        // A rule with no `output` gets the pin `sticky-follow` asks for.
        let message = business.reload_from_disk().await.unwrap();
        assert!(message.contains("2 window(s) re-pinned"), "{message}");
        assert_eq!(business.pin_of(1).await, vec!["DP-2".to_string()]);
        assert_eq!(business.pin_of(2).await, vec!["DP-1".to_string()]);

        // A tracked window that closed has no pin to re-evaluate. The window
        // that stays keeps the workspace it is on.
        niri.set_windows(&[(2, "foot", "Terminal")]);
        niri.set_window_workspace(2, 1);
        let message = business.reload_from_disk().await.unwrap();
        assert!(message.contains("0 window(s) re-pinned"), "{message}");

        // And a window no rule matches any more keeps the pin it had.
        config_file.write("sticky-follow = \"own-output\"\n");
        let message = business.reload_from_disk().await.unwrap();
        assert!(message.contains("0 window(s) re-pinned"), "{message}");
        assert_eq!(business.pin_of(2).await, vec!["DP-1".to_string()]);
    }

    #[tokio::test]
    async fn test_workspace_activation_stops_when_the_layout_is_unreadable() {
        let niri = two_monitors();
        let business = business(&niri);
        business.add_sticky_window(1).await.unwrap();

        niri.fail_next(Call::Workspaces);
        let (result, logs) = with_logs(business.handle_workspace_activation(2)).await;

        assert!(result.is_ok(), "{result:?}");
        assert!(
            reported(&logs, "injected failure for workspaces"),
            "{logs:?}"
        );
        assert!(niri.moves().is_empty(), "nothing moves without a layout");
    }

    #[tokio::test]
    async fn test_workspace_activation_keeps_going_when_a_move_is_refused() {
        let niri = two_monitors();
        niri.set_window_workspace(1, 1); // DP-1, but pinned to DP-2
        niri.set_window_workspace(2, 1);
        let business = business_with_config(
            &niri,
            r#"
[sticky.browser]
app-id = "foot"
output = "DP-2"
"#,
        );
        for id in [1, 2] {
            business.add_sticky_window(id).await.unwrap();
        }

        niri.fail_move(1);
        let (result, logs) = with_logs(business.handle_workspace_activation(2)).await;

        assert!(result.is_ok(), "{result:?}");
        assert!(reported(&logs, "refused to move window 1"), "{logs:?}");
        assert_eq!(
            niri.moved_to_id(2),
            vec![2],
            "the other window still follows"
        );
    }

    #[tokio::test]
    async fn test_toggling_by_appid_brings_staged_windows_back() {
        let niri = with_layout();
        let business = business(&niri);
        business.add_sticky_window(1).await.unwrap();
        business.stage_window(1).await.unwrap();

        let outcome = business.toggle_by_appid("foot").await.unwrap();

        assert_eq!(outcome.succeeded, vec![1, 2], "both terminals toggled");
        assert!(!staged(&business, 1).await);
        assert!(sticky(&business, 1).await, "it was sticky before the stage");
        assert_eq!(niri.moved_to_id(1), vec![1], "back on the active workspace");

        let outcome = business.toggle_by_appid("foot").await.unwrap();

        assert_eq!(outcome.succeeded, vec![1, 2]);
        assert!(!sticky(&business, 1).await, "the second toggle unsticks it");
    }

    #[tokio::test]
    async fn test_toggling_by_appid_reports_windows_it_could_not_move() {
        let niri = with_layout();
        let business = business(&niri);
        business.stage_window(1).await.unwrap();

        niri.fail_move(1);
        let outcome = business.toggle_by_appid("foot").await.unwrap();

        assert_eq!(outcome.failed, vec![1], "the refused window is reported");
        assert_eq!(outcome.succeeded, vec![2]);
        assert!(staged(&business, 1).await, "it stays on the stage");
        assert!(!sticky(&business, 1).await);
    }

    #[tokio::test]
    async fn test_toggling_the_stage_by_appid_and_title() {
        let niri = with_layout();
        let business = business(&niri);

        let housed = business.toggle_stage_by_appid("foot", 1).await.unwrap();
        assert_eq!(housed.succeeded, vec![1, 2]);
        assert!(staged(&business, 1).await && staged(&business, 2).await);

        let restored = business.toggle_stage_by_appid("foot", 1).await.unwrap();
        assert_eq!(restored.succeeded, vec![1, 2]);
        assert!(!staged(&business, 1).await && !staged(&business, 2).await);

        let by_title = business.toggle_stage_by_title("Terminal", 1).await.unwrap();
        assert_eq!(
            by_title.succeeded,
            vec![1, 2],
            "both terminals match the title"
        );
        assert!(staged(&business, 1).await);

        niri.fail_move(2);
        let partial = business.toggle_stage_by_title("Terminal", 1).await.unwrap();
        assert_eq!(partial.succeeded, vec![1], "only the one that moved");
        assert_eq!(partial.failed, vec![2], "the refused one is reported");
        assert!(!staged(&business, 1).await);
        assert!(staged(&business, 2).await, "it stays parked");
    }

    #[tokio::test]
    async fn test_staging_a_window_that_is_already_staged_changes_nothing() {
        let niri = with_layout();
        let business = business(&niri);
        business.stage_window(1).await.unwrap();
        let moves = niri.moves();

        business.stage_window(1).await.unwrap();

        assert!(staged(&business, 1).await);
        assert_eq!(niri.moves(), moves, "no second move was needed");
    }

    #[tokio::test]
    async fn test_staging_and_unstaging_reject_windows_niri_does_not_know() {
        let niri = with_layout();
        let business = business(&niri);

        for request in [
            Request::Stage { window_id: 999 },
            Request::Unstage { window_id: 999 },
        ] {
            match business.handle_request(request.clone()).await {
                Response::Error { message } => {
                    assert_eq!(message, "Window not found in Niri", "{request:?}")
                }
                other => panic!("expected an error for {request:?}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn test_toggling_a_window_that_is_gone_reports_it() {
        let niri = with_layout();
        let business = business(&niri);
        niri.set_active_window(999);

        let error = business.toggle_active_window().await.unwrap_err();

        assert_eq!(error.to_string(), "Active window not found in Niri");
        assert!(niri.moves().is_empty());
    }

    #[tokio::test]
    async fn test_auto_sticky_does_not_take_a_staged_window_over() {
        let niri = with_layout();
        let business = business_with_config(
            &niri,
            r#"
[sticky.terminals]
app-id = "foot"
"#,
        );
        business.stage_window(1).await.unwrap();

        let (result, logs) = with_logs(business.handle_window_opened_or_changed(
            1,
            Some("foot".into()),
            Some("Terminal".into()),
            false,
        ))
        .await;

        result.unwrap();
        assert!(reported(&logs, "Auto-sticky window 1"), "{logs:?}");
        assert!(staged(&business, 1).await, "the rule does not unstage it");
        assert!(!sticky(&business, 1).await, "a window is sticky or staged");
    }

    #[tokio::test]
    async fn test_removing_and_toggling_report_what_changed() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = business(&niri);
        niri.set_active_window(1);

        assert_eq!(
            business.handle_request(Request::ToggleActive).await,
            Response::success("Added active window to sticky")
        );
        assert_eq!(
            business.handle_request(Request::ToggleActive).await,
            Response::success("Removed active window from sticky")
        );

        business.handle_request(Request::Add { window_id: 2 }).await;
        assert_eq!(
            business
                .handle_request(Request::Remove { window_id: 2 })
                .await,
            Response::success("Removed")
        );
        assert_eq!(
            business
                .handle_request(Request::Remove { window_id: 2 })
                .await,
            Response::success("Not in sticky list")
        );
    }

    #[tokio::test]
    async fn test_toggling_by_appid_and_title_reports_the_windows() {
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = business(&niri);

        assert_eq!(
            business
                .handle_request(Request::ToggleAppid {
                    appid: "foot".into()
                })
                .await,
            Response::success("Toggled 2 window(s)")
        );
        assert_eq!(
            business
                .handle_request(Request::ToggleTitle {
                    title: "Gmail".into()
                })
                .await,
            Response::success("Toggled 1 window(s)")
        );
    }

    #[tokio::test]
    async fn test_stage_toggle_requests_report_their_outcome() {
        let niri = with_layout();
        let business = business(&niri);

        assert_eq!(
            business
                .handle_request(Request::StageToggleAppid {
                    appid: "foot".into()
                })
                .await,
            Response::success("Toggled 2 window(s)")
        );
        assert!(staged(&business, 1).await && staged(&business, 2).await);

        assert_eq!(
            business
                .handle_request(Request::StageToggleTitle {
                    title: "Terminal".into()
                })
                .await,
            Response::success("Toggled 2 window(s)")
        );
        assert!(!staged(&business, 1).await && !staged(&business, 2).await);
    }

    #[tokio::test]
    async fn test_stage_toggle_active_parks_and_restores_the_active_window() {
        let niri = with_layout();
        let business = business(&niri);
        niri.set_active_window(1);

        assert_eq!(
            business.handle_request(Request::StageToggleActive).await,
            Response::success("Staged active window")
        );
        assert!(staged(&business, 1).await);

        assert_eq!(
            business.handle_request(Request::StageToggleActive).await,
            Response::success("Unstaged active window")
        );
        assert!(!staged(&business, 1).await);
    }

    #[tokio::test]
    async fn test_requests_report_a_failed_workspace_lookup() {
        let niri = with_layout();
        let business = business(&niri);
        niri.set_active_window(1);

        // These need the active workspace before they can do anything.
        for request in [
            Request::Unstage { window_id: 1 },
            Request::StageToggleAppid {
                appid: "foot".into(),
            },
            Request::StageToggleTitle {
                title: "Terminal".into(),
            },
            Request::UnstageAll,
        ] {
            niri.fail_next(Call::ActiveWorkspace);
            match business.handle_request(request.clone()).await {
                Response::Error { message } => {
                    assert!(
                        message.contains("Failed to get active workspace ID"),
                        "{request:?}: {message}"
                    )
                }
                other => panic!("expected an error for {request:?}, got {other:?}"),
            }
        }

        // A window that is already parked needs it too.
        business.stage_window(1).await.unwrap();
        niri.fail_next(Call::ActiveWorkspace);
        match business.handle_request(Request::StageToggleActive).await {
            Response::Error { message } => {
                assert!(
                    message.contains("Failed to get active workspace ID"),
                    "{message}"
                )
            }
            other => panic!("expected an error, got {other:?}"),
        }

        // And the focused window is looked up first, with its own message.
        niri.fail_next(Call::ActiveWindow);
        match business.handle_request(Request::StageToggleActive).await {
            Response::Error { message } => {
                assert!(message.contains("Failed to get active window"), "{message}")
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_reload_request_applies_the_file_and_reports_a_broken_one() {
        let config_file = TempConfig::new();
        config_file.write("[sticky.terminals]\napp-id = \"foot\"\n");
        let niri = Arc::new(FakeNiri::with_windows(WINDOWS));
        let business = BusinessLogic::new(
            Config::from_toml("").unwrap(),
            config_file.path(),
            niri,
            storage(),
        );

        match business.handle_request(Request::Reload).await {
            Response::Success { message } => assert!(message.contains("1 rule(s)"), "{message}"),
            other => panic!("expected success, got {other:?}"),
        }

        config_file.write("this is not toml");
        match business.handle_request(Request::Reload).await {
            Response::Error { message } => assert!(
                message.contains("Keeping the running configuration"),
                "{message}"
            ),
            other => panic!("expected an error, got {other:?}"),
        }
    }
}
