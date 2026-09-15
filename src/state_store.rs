//! Persisted daemon state.
//!
//! Without it, restarting the daemon (or the session) forgets which windows are
//! sticky and which are parked on the stage workspace: the windows stay where
//! they were, but nsticky no longer tracks them, so `stage restore` lists
//! nothing.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// On-disk format version, so a future change can migrate instead of guessing.
pub const STATE_VERSION: u32 = 1;

/// A sticky window as stored on disk. Files written before output pinning
/// existed list bare ids, so both shapes are accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StoredSticky {
    /// The bare window id, no pin.
    Id(u64),
    /// Window id plus the outputs it is pinned to, in preference order.
    Pinned { id: u64, outputs: Vec<String> },
}

impl StoredSticky {
    pub fn new(id: u64, outputs: Vec<String>) -> Self {
        if outputs.is_empty() {
            Self::Id(id)
        } else {
            Self::Pinned { id, outputs }
        }
    }

    /// Id and output pin, whichever shape this entry has.
    pub fn into_parts(self) -> (u64, Vec<String>) {
        match self {
            Self::Id(id) => (id, Vec::new()),
            Self::Pinned { id, outputs } => (id, outputs),
        }
    }

    pub fn id(&self) -> u64 {
        match self {
            Self::Id(id) => *id,
            Self::Pinned { id, .. } => *id,
        }
    }
}

/// A scratchpad window as stored on disk. Files written by an earlier version
/// kept only the window id, so both shapes are accepted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StoredScratchpad {
    Id(u64),
    Full {
        id: u64,
        #[serde(default)]
        was_sticky: bool,
        #[serde(default)]
        outputs: Vec<String>,
        #[serde(default)]
        size: Option<(i32, i32)>,
        #[serde(default)]
        position: Option<(f64, f64)>,
    },
}

/// A parked scratchpad window, whatever shape the file on disk had.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScratchpadRecord {
    pub id: u64,
    pub was_sticky: bool,
    pub outputs: Vec<String>,
    pub size: Option<(i32, i32)>,
    pub position: Option<(f64, f64)>,
}

impl StoredScratchpad {
    pub fn into_record(self) -> ScratchpadRecord {
        match self {
            Self::Id(id) => ScratchpadRecord {
                id,
                ..ScratchpadRecord::default()
            },
            Self::Full {
                id,
                was_sticky,
                outputs,
                size,
                position,
            } => ScratchpadRecord {
                id,
                was_sticky,
                outputs,
                size,
                position,
            },
        }
    }

    pub fn id(&self) -> u64 {
        match self {
            Self::Id(id) => *id,
            Self::Full { id, .. } => *id,
        }
    }
}

/// A staged window as stored on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredStaged {
    pub id: u64,
    /// Whether the window was sticky before it was staged.
    pub was_sticky: bool,
    /// Output pin to restore with the window.
    #[serde(default)]
    pub outputs: Vec<String>,
    /// Size it had when it was parked, so showing it puts it back exactly.
    #[serde(default)]
    pub size: Option<(i32, i32)>,
    /// Position it had when it was parked.
    #[serde(default)]
    pub position: Option<(f64, f64)>,
}

/// The daemon state as stored on disk.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StoredState {
    pub version: u32,
    pub sticky: Vec<StoredSticky>,
    pub staged: Vec<StoredStaged>,
    /// Scratchpad name to the window it parked.
    #[serde(default)]
    pub scratchpads: std::collections::BTreeMap<String, StoredScratchpad>,
}

impl StoredState {
    pub fn new(sticky: Vec<StoredSticky>, staged: Vec<StoredStaged>) -> Self {
        Self {
            version: STATE_VERSION,
            sticky,
            staged,
            scratchpads: std::collections::BTreeMap::new(),
        }
    }
}

/// Where the daemon keeps its state between runs.
pub trait StateStorage: Send + Sync {
    /// Stored state, or `None` when nothing was stored yet.
    fn load(&self) -> Result<Option<StoredState>>;
    fn save(&self, state: &StoredState) -> Result<()>;
}

/// `$XDG_STATE_HOME/nsticky/state.json` (or the platform equivalent).
pub fn default_path() -> PathBuf {
    state_path(dirs::state_dir(), dirs::data_dir())
}

/// Both directories are arguments so the precedence rules are testable without
/// environment variables.
fn state_path(state_dir: Option<PathBuf>, data_dir: Option<PathBuf>) -> PathBuf {
    state_dir
        .or(data_dir)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("nsticky")
        .join("state.json")
}

/// Storage backed by a JSON file, written atomically.
#[derive(Debug, Clone)]
pub struct FileStorage {
    path: PathBuf,
}

impl FileStorage {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl StateStorage for FileStorage {
    fn load(&self) -> Result<Option<StoredState>> {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| format!("Failed to read state: {:?}", self.path));
            }
        };

        let state: StoredState = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse state: {:?}", self.path))?;

        if state.version != STATE_VERSION {
            bail!(
                "Unsupported state version {} in {:?} (expected {STATE_VERSION})",
                state.version,
                self.path
            );
        }

        Ok(Some(state))
    }

    fn save(&self, state: &StoredState) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("Failed to create state directory: {dir:?}"))?;
        }

        // Write-then-rename: a crash mid-write never leaves a truncated state
        // file behind.
        let temp = self.path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(state)?;
        std::fs::write(&temp, json).with_context(|| format!("Failed to write state: {temp:?}"))?;
        std::fs::rename(&temp, &self.path)
            .with_context(|| format!("Failed to replace state: {:?}", self.path))?;
        Ok(())
    }
}

#[cfg(test)]
pub use memory::MemoryStorage;

#[cfg(test)]
mod memory {
    use super::*;
    use parking_lot::Mutex;

    /// Storage that keeps everything in memory and can be told to fail.
    #[derive(Debug, Default)]
    pub struct MemoryStorage {
        state: Mutex<Option<StoredState>>,
        saves: Mutex<usize>,
        fail: Mutex<bool>,
    }

    impl MemoryStorage {
        pub fn new() -> Self {
            Self::default()
        }

        /// Storage holding an initial state, as if left by a previous run.
        pub fn with_state(state: StoredState) -> Self {
            let storage = Self::new();
            *storage.state.lock() = Some(state);
            storage
        }

        pub fn fail(&self, fail: bool) {
            *self.fail.lock() = fail;
        }

        /// How many times state was written.
        pub fn saves(&self) -> usize {
            *self.saves.lock()
        }

        pub fn snapshot(&self) -> Option<StoredState> {
            self.state.lock().clone()
        }
    }

    impl StateStorage for MemoryStorage {
        fn load(&self) -> Result<Option<StoredState>> {
            if *self.fail.lock() {
                bail!("memory storage is failing");
            }
            Ok(self.state.lock().clone())
        }

        fn save(&self, state: &StoredState) -> Result<()> {
            if *self.fail.lock() {
                bail!("memory storage is failing");
            }
            *self.saves.lock() += 1;
            *self.state.lock() = Some(state.clone());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of its own, so a test can remove it wholesale.
    ///
    /// The sequence number matters: the clock a `SystemTime` reads can tick
    /// coarsely enough for two tests to start in the same nanosecond, and one
    /// of these tests takes its directory away (`remove_dir_all`) or makes it
    /// read-only while another is still creating files in it.
    fn temp_dir() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

        let unique = format!(
            "nsticky-state-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        std::env::temp_dir().join(unique)
    }

    fn temp_path() -> PathBuf {
        temp_dir().join("state.json")
    }

    /// Storage whose file already holds `content`, plus that file's path.
    fn storage_with_content(content: &str) -> (FileStorage, PathBuf) {
        let path = temp_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        (FileStorage::new(path.clone()), path)
    }

    #[test]
    fn test_load_without_a_file_is_none() {
        let storage = FileStorage::new(temp_path());
        assert_eq!(storage.load().unwrap(), None);
    }

    #[test]
    fn test_roundtrip() {
        let path = temp_path();
        let storage = FileStorage::new(path.clone());

        let state = StoredState::new(
            vec![
                StoredSticky::Id(4),
                StoredSticky::new(2, vec!["DP-1".into()]),
            ],
            vec![StoredStaged {
                id: 7,
                was_sticky: true,
                outputs: vec!["DP-2".into()],
                size: Some((1100, 480)),
                position: Some((730.0, 94.0)),
            }],
        );
        storage.save(&state).unwrap();

        assert_eq!(storage.load().unwrap(), Some(state));

        // Writes are atomic: no temp file is left behind.
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_save_replaces_the_previous_state() {
        let path = temp_path();
        let storage = FileStorage::new(path.clone());

        storage
            .save(&StoredState::new(
                vec![
                    StoredSticky::Id(1),
                    StoredSticky::Id(2),
                    StoredSticky::Id(3),
                ],
                Vec::new(),
            ))
            .unwrap();
        storage
            .save(&StoredState::new(vec![StoredSticky::Id(1)], Vec::new()))
            .unwrap();

        assert_eq!(
            storage.load().unwrap(),
            Some(StoredState::new(vec![StoredSticky::Id(1)], Vec::new()))
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_corrupt_state_is_an_error() {
        let path = temp_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        let storage = FileStorage::new(path.clone());

        let error = storage.load().unwrap_err();
        assert!(
            format!("{error:#}").contains("Failed to parse state"),
            "{error:#}"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_unknown_version_is_rejected() {
        let path = temp_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"version":99,"sticky":[1],"staged":[]}"#).unwrap();
        let storage = FileStorage::new(path.clone());

        let error = storage.load().unwrap_err();
        assert!(
            format!("{error:#}").contains("Unsupported state version 99"),
            "{error:#}"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_default_path_is_absolute() {
        let path = default_path();
        assert!(path.is_absolute(), "{path:?}");
        assert!(path.ends_with("nsticky/state.json"), "{path:?}");
    }

    #[test]
    fn test_state_keeps_scratchpad_slots_and_geometry() {
        let mut state = StoredState::new(Vec::new(), Vec::new());
        state
            .scratchpads
            .insert("term".to_string(), StoredScratchpad::Id(42));
        let path = temp_path();
        let storage = FileStorage::new(path.clone());
        storage.save(&state).unwrap();

        assert_eq!(
            storage.load().unwrap().unwrap().scratchpads["term"],
            StoredScratchpad::Id(42)
        );

        // Files without the new fields still load.
        std::fs::write(&path, r#"{"version":1,"sticky":[],"staged":[]}"#).unwrap();
        let loaded = storage.load().unwrap().unwrap();
        assert!(loaded.scratchpads.is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_bare_ids_from_older_state_files_still_load() {
        let path = temp_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"version":1,"sticky":[1,2],"staged":[{"id":3,"was_sticky":true}]}"#,
        )
        .unwrap();
        let storage = FileStorage::new(path.clone());

        let state = storage.load().unwrap().expect("state loads");
        assert_eq!(state.sticky, vec![StoredSticky::Id(1), StoredSticky::Id(2)]);
        assert!(state.staged[0].outputs.is_empty());

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_stored_sticky_keeps_pins_and_shape() {
        assert_eq!(StoredSticky::new(1, Vec::new()), StoredSticky::Id(1));
        assert_eq!(
            StoredSticky::new(2, vec!["DP-1".into()]),
            StoredSticky::Pinned {
                id: 2,
                outputs: vec!["DP-1".into()]
            }
        );
        assert_eq!(
            StoredSticky::new(2, vec!["DP-1".into()]).into_parts(),
            (2, vec!["DP-1".to_string()])
        );
    }

    #[test]
    fn test_memory_storage_tracks_saves() {
        let storage = MemoryStorage::new();
        assert_eq!(storage.saves(), 0);

        let state = StoredState::new(vec![StoredSticky::Id(1)], Vec::new());
        storage.save(&state).unwrap();

        assert_eq!(storage.saves(), 1);
        assert_eq!(storage.snapshot(), Some(state));
    }

    #[test]
    fn test_memory_storage_can_fail() {
        let storage = MemoryStorage::new();
        storage.fail(true);

        assert!(storage.load().is_err());
        assert!(storage.save(&StoredState::default()).is_err());
    }

    #[test]
    fn test_valid_json_of_the_wrong_shape_is_rejected() {
        for content in [
            // A JSON document that is not an object at all.
            "[]",
            // Truncated mid-write.
            "{\"version\":1,\"sticky\":[{",
            // Wrong types and missing fields.
            "{\"version\":1,\"sticky\":\"none\",\"staged\":[]}",
            "{\"sticky\":[],\"staged\":[]}",
            "{\"version\":1,\"sticky\":[{\"outputs\":[\"DP-1\"]}],\"staged\":[]}",
            "{\"version\":1,\"sticky\":[{\"id\":\"one\",\"outputs\":[]}],\"staged\":[]}",
            // was_sticky is what tells a staged window apart, so it is required.
            "{\"version\":1,\"sticky\":[],\"staged\":[{\"id\":3}]}",
            "{\"version\":1,\"sticky\":[],\"staged\":[],\"scratchpads\":{\"term\":{}}}",
        ] {
            let (storage, path) = storage_with_content(content);
            let error = storage.load().unwrap_err();
            assert!(
                format!("{error:#}").contains("Failed to parse state"),
                "{content} loaded: {error:#}"
            );
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }
    }

    #[test]
    fn test_state_path_holding_something_that_is_not_a_file_is_an_error() {
        let path = temp_path();
        // A directory: reading it fails for a reason other than "no state yet",
        // which must not be reported as "nothing stored".
        std::fs::create_dir_all(&path).unwrap();
        let storage = FileStorage::new(path.clone());

        let error = storage.load().unwrap_err();
        assert!(
            format!("{error:#}").contains("Failed to read state"),
            "{error:#}"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_save_creates_the_missing_state_directory() {
        let root = temp_dir();
        let path = root.join("nested").join("deeper").join("state.json");
        let storage = FileStorage::new(path.clone());
        assert!(!path.parent().unwrap().exists());

        let state = StoredState::new(vec![StoredSticky::Id(5)], Vec::new());
        storage.save(&state).unwrap();

        assert!(path.exists());
        assert_eq!(storage.load().unwrap(), Some(state));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_save_where_the_directory_cannot_be_created_is_an_error() {
        let root = temp_dir();
        std::fs::create_dir_all(&root).unwrap();
        // A regular file where the state directory should be.
        std::fs::write(root.join("blocked"), b"in the way").unwrap();
        let storage = FileStorage::new(root.join("blocked").join("state.json"));

        let error = storage.save(&StoredState::default()).unwrap_err();
        assert!(
            format!("{error:#}").contains("Failed to create state directory"),
            "{error:#}"
        );
        assert!(!root.join("blocked").join("state.json").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_save_into_an_unwritable_directory_reports_writing() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_dir();
        std::fs::create_dir_all(&root).unwrap();

        let writable = std::fs::metadata(&root).unwrap().permissions();
        let mut read_only = writable.clone();
        read_only.set_mode(0o555);
        std::fs::set_permissions(&root, read_only).unwrap();

        let storage = FileStorage::new(root.join("state.json"));
        let result = storage.save(&StoredState::default());

        std::fs::set_permissions(&root, writable).unwrap();
        // Running as root writes through the mode bits, so there is nothing to
        // assert about that case.
        if result.is_ok() {
            let _ = std::fs::remove_dir_all(&root);
            return;
        }

        let error = result.unwrap_err();
        assert!(
            format!("{error:#}").contains("Failed to write state"),
            "{error:#}"
        );
        assert!(!root.join("state.json").exists(), "nothing was written");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_failed_save_keeps_the_previous_state_file() {
        let root = temp_dir();
        let storage = FileStorage::new(root.join("state.json"));
        let previous = StoredState::new(vec![StoredSticky::Id(1)], Vec::new());
        storage.save(&previous).unwrap();

        // The temp file cannot be written, so the state file must survive
        // untouched rather than being truncated or half-replaced.
        std::fs::create_dir_all(root.join("state.json.tmp")).unwrap();
        let error = storage
            .save(&StoredState::new(vec![StoredSticky::Id(2)], Vec::new()))
            .unwrap_err();

        assert!(
            format!("{error:#}").contains("Failed to write state"),
            "{error:#}"
        );
        assert_eq!(storage.load().unwrap(), Some(previous));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_save_without_a_parent_directory_still_reports_the_failure() {
        // The root path has no parent, so the directory step is skipped and the
        // write is where it fails. An error, not a panic and not a silent no-op.
        let storage = FileStorage::new(PathBuf::from("/"));

        let error = storage.save(&StoredState::default()).unwrap_err();
        assert!(
            format!("{error:#}").contains("Failed to write state"),
            "{error:#}"
        );
    }

    #[test]
    fn test_save_over_a_path_that_is_not_replaceable_is_an_error() {
        let root = temp_dir();
        // A directory where the state file goes: the temp file is written, the
        // rename on top of it fails.
        std::fs::create_dir_all(root.join("state.json")).unwrap();
        let storage = FileStorage::new(root.join("state.json"));

        let error = storage.save(&StoredState::default()).unwrap_err();
        assert!(
            format!("{error:#}").contains("Failed to replace state"),
            "{error:#}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_default_path_prefers_the_state_dir_and_falls_back_to_data_dir() {
        assert_eq!(
            state_path(Some("/state".into()), Some("/data".into())),
            PathBuf::from("/state/nsticky/state.json")
        );
        assert_eq!(
            state_path(None, Some("/data".into())),
            PathBuf::from("/data/nsticky/state.json")
        );
        assert_eq!(
            state_path(None, None),
            PathBuf::from("/tmp/nsticky/state.json"),
            "a platform that reports no home still gets a usable path"
        );
    }

    #[test]
    fn test_scratchpad_stored_by_id_alone_round_trips() {
        let root = temp_dir();
        let storage = FileStorage::new(root.join("state.json"));

        let mut state = StoredState::new(Vec::new(), Vec::new());
        state
            .scratchpads
            .insert("term".to_string(), StoredScratchpad::Id(42));
        storage.save(&state).unwrap();

        let raw = std::fs::read_to_string(root.join("state.json")).unwrap();
        assert!(
            raw.contains("\"term\": 42"),
            "an id-only scratchpad stays a bare id: {raw}"
        );

        let stored = &storage.load().unwrap().unwrap().scratchpads["term"];
        assert_eq!(stored.id(), 42);
        assert_eq!(
            stored.clone().into_record(),
            ScratchpadRecord {
                id: 42,
                ..ScratchpadRecord::default()
            }
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_scratchpad_geometry_round_trips() {
        let root = temp_dir();
        let storage = FileStorage::new(root.join("state.json"));
        let record = ScratchpadRecord {
            id: 7,
            was_sticky: true,
            outputs: vec!["DP-2".to_string()],
            size: Some((1200, 600)),
            position: Some((12.0, 34.0)),
        };

        let mut state = StoredState::new(Vec::new(), Vec::new());
        state.scratchpads.insert(
            "music".to_string(),
            StoredScratchpad::Full {
                id: record.id,
                was_sticky: record.was_sticky,
                outputs: record.outputs.clone(),
                size: record.size,
                position: record.position,
            },
        );
        storage.save(&state).unwrap();

        let stored = &storage.load().unwrap().unwrap().scratchpads["music"];
        assert_eq!(stored.id(), 7);
        assert_eq!(stored.clone().into_record(), record);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_scratchpad_file_without_the_optional_fields_uses_defaults() {
        let (storage, path) = storage_with_content(
            r#"{"version":1,"sticky":[],"staged":[],"scratchpads":{"term":{"id":9}}}"#,
        );

        let stored = &storage.load().unwrap().unwrap().scratchpads["term"];
        assert_eq!(
            stored.clone().into_record(),
            ScratchpadRecord {
                id: 9,
                ..ScratchpadRecord::default()
            },
            "geometry and pin are optional in the file"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
