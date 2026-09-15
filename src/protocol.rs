use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::PathBuf;

/// Socket path used when `XDG_RUNTIME_DIR` is unavailable.
const FALLBACK_SOCKET_PATH: &str = "/tmp/niri_sticky_cli.sock";

/// Socket the daemon listens on and the CLI connects to.
///
/// Prefers `$XDG_RUNTIME_DIR/nsticky/cli.sock`, a per-user directory other
/// users cannot pre-create, instead of a fixed path in the shared `/tmp`.
pub fn cli_socket_path() -> PathBuf {
    socket_path_in(std::env::var_os("XDG_RUNTIME_DIR"))
}

/// Where the socket lives under a given `XDG_RUNTIME_DIR`.
///
/// Split out so the fallback can be tested without touching the environment of
/// the whole test process.
fn socket_path_in(runtime_dir: Option<OsString>) -> PathBuf {
    match runtime_dir.filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("nsticky").join("cli.sock"),
        None => PathBuf::from(FALLBACK_SOCKET_PATH),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    Add {
        window_id: u64,
    },
    Remove {
        window_id: u64,
    },
    List,
    ToggleActive,
    ToggleAppid {
        appid: String,
    },
    ToggleTitle {
        title: String,
    },
    StageList,
    Stage {
        window_id: u64,
    },
    Unstage {
        window_id: u64,
    },
    StageToggleActive,
    StageToggleAppid {
        appid: String,
    },
    StageToggleTitle {
        title: String,
    },
    StageAll,
    UnstageAll,
    Windows,
    /// Re-read `config.toml` and apply it without restarting the daemon.
    Reload,
    /// Toggle a scratchpad: hide it, show it, or start it. Without a name it
    /// toggles the focused window.
    Scratchpad {
        name: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Success { message: String },
    Error { message: String },
    Data { data: String },
}

impl Response {
    pub fn success(message: impl Into<String>) -> Self {
        Self::Success {
            message: message.into(),
        }
    }

    /// Renders the full error chain, so context added by `anyhow::Context` is
    /// visible to the user.
    pub fn error(error: impl std::fmt::Display) -> Self {
        Self::Error {
            message: format!("{error:#}"),
        }
    }

    pub fn data(data: impl Into<String>) -> Self {
        Self::Data { data: data.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_socket_path_follows_xdg_runtime_dir() {
        assert_eq!(
            socket_path_in(Some(OsString::from("/run/user/1000"))),
            PathBuf::from("/run/user/1000/nsticky/cli.sock")
        );
    }

    #[test]
    fn test_socket_path_falls_back_when_xdg_runtime_dir_is_missing_or_empty() {
        // An empty value is what a script that forgot to export the variable
        // leaves behind; joining it would give a relative path under the
        // daemon's working directory.
        assert_eq!(
            socket_path_in(None),
            PathBuf::from("/tmp/niri_sticky_cli.sock")
        );
        assert_eq!(
            socket_path_in(Some(OsString::new())),
            PathBuf::from("/tmp/niri_sticky_cli.sock")
        );
    }

    #[test]
    fn test_socket_path_keeps_a_relative_runtime_dir_relative() {
        assert_eq!(
            socket_path_in(Some(OsString::from("run"))),
            PathBuf::from("run/nsticky/cli.sock")
        );
    }

    #[test]
    fn test_request_roundtrip_add() {
        let request = Request::Add { window_id: 7 };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(json, r#"{"command":"add","window_id":7}"#);
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
    }

    #[test]
    fn test_request_roundtrip_toggle_active() {
        let request = Request::ToggleActive;
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(json, r#"{"command":"toggle_active"}"#);
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
    }

    #[test]
    fn test_request_roundtrip_stage_toggle_appid() {
        let request = Request::StageToggleAppid {
            appid: "firefox".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            json,
            r#"{"command":"stage_toggle_appid","appid":"firefox"}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
    }

    #[test]
    fn test_response_roundtrip_success() {
        let response = Response::success("Added");
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(json, r#"{"status":"success","message":"Added"}"#);
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), response);
    }

    #[test]
    fn test_response_roundtrip_error() {
        let response = Response::error("boom");
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(json, r#"{"status":"error","message":"boom"}"#);
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), response);
    }

    #[test]
    fn test_response_roundtrip_data() {
        let response = Response::data("[1,2]");
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(json, r#"{"status":"data","data":"[1,2]"}"#);
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), response);
    }

    #[test]
    fn test_error_renders_the_whole_context_chain() {
        use anyhow::Context;
        let error = Err::<(), _>(anyhow::anyhow!("No such file or directory"))
            .context("Failed to get active workspace ID")
            .unwrap_err();
        assert_eq!(
            Response::error(error),
            Response::Error {
                message: "Failed to get active workspace ID: No such file or directory".to_string()
            }
        );
    }
}
