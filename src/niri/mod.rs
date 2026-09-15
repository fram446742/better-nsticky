//! Niri compositor access.
//!
//! [`Niri`] is the seam between nsticky's bookkeeping and the compositor:
//! production code uses [`NiriClient`] (JSON IPC over `$NIRI_SOCKET`), tests use
//! `fake::FakeNiri`, compiled only for tests.

pub mod client;
pub mod events;
#[cfg(test)]
pub mod fake;

use std::pin::Pin;

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub use client::NiriClient;

/// Boxed future returned by [`Niri`] methods, used as a trait object.
pub type NiriFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// A window as reported by Niri.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowInfo {
    pub id: u64,
    pub app_id: Option<String>,
    pub title: Option<String>,
    /// Workspace the window lives on, when niri reports one.
    #[serde(default)]
    pub workspace_id: Option<u64>,
    #[serde(default)]
    pub floating: bool,
    /// Logical pixels, as niri reports them.
    #[serde(default)]
    pub size: Option<(i32, i32)>,
    /// Top-left position on screen, as niri reports it.
    #[serde(default)]
    pub position: Option<(f64, f64)>,
}

/// A workspace as reported by Niri.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceInfo {
    pub id: u64,
    /// Position of the workspace on its output, top to bottom.
    #[serde(default)]
    pub idx: u8,
    pub name: Option<String>,
    /// Output the workspace lives on, when niri reports one.
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub is_active: bool,
}

impl WorkspaceInfo {
    /// A workspace with everything optional unset, for tests and fixtures.
    #[cfg(test)]
    pub fn default_for_tests() -> Self {
        Self {
            id: 0,
            idx: 0,
            name: None,
            output: None,
            is_active: false,
        }
    }
}

/// Size a window can be given, as niri's IPC expresses it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Size {
    /// Logical pixels.
    Fixed(i32),
    /// Percentage of the working area, as niri's `SetProportion` expects it
    /// (60.0 is 60%).
    Percent(f64),
}

/// Queries and actions nsticky performs against the compositor.
///
/// Kept deliberately small: everything else (matching, bookkeeping, ordering)
/// lives in [`crate::business`] and is tested against `fake::FakeNiri`.
pub trait Niri: Send + Sync {
    /// Every open window.
    fn windows(&self) -> NiriFuture<'_, Vec<WindowInfo>>;
    /// Every workspace.
    fn workspaces(&self) -> NiriFuture<'_, Vec<WorkspaceInfo>>;
    /// Id of the focused window.
    fn active_window(&self) -> NiriFuture<'_, u64>;
    /// Id of the focused workspace.
    fn active_workspace(&self) -> NiriFuture<'_, u64>;
    /// Move a window to a workspace without focusing it.
    fn move_window<'a>(&'a self, window_id: u64, workspace_id: u64) -> NiriFuture<'a, ()>;
    /// Name a workspace without focusing it.
    fn name_workspace<'a>(&'a self, name: &'a str, workspace_id: u64) -> NiriFuture<'a, ()>;
    /// Drop the name of a workspace, so niri reclaims it once it is empty.
    fn unname_workspace<'a>(&'a self, workspace_id: u64) -> NiriFuture<'a, ()>;
    /// Start a command (argv, no shell involved).
    fn spawn<'a>(&'a self, command: &'a [String]) -> NiriFuture<'a, ()>;
    /// Focus a window without moving it.
    fn focus_window(&self, window_id: u64) -> NiriFuture<'_, ()>;
    /// Put a window in the floating layout.
    fn float_window(&self, window_id: u64) -> NiriFuture<'_, ()>;
    /// Move a floating window, in logical pixels from the working area's corner.
    fn move_floating_window(&self, window_id: u64, x: f64, y: f64) -> NiriFuture<'_, ()>;
    /// Move a workspace to a position on its output (1-based), without focusing
    /// it.
    fn move_workspace_to_index(&self, workspace_id: u64, index: usize) -> NiriFuture<'_, ()>;
    /// Resize a window, in pixels or as a proportion of the working area.
    fn resize_window(
        &self,
        window_id: u64,
        width: Option<Size>,
        height: Option<Size>,
    ) -> NiriFuture<'_, ()>;
}
