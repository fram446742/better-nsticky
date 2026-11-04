//! Niri event stream: the events nsticky reacts to and the connection that
//! delivers them.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::Path;

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

/// Niri events nsticky reacts to.
#[derive(Debug, Clone)]
pub enum NiriEvent {
    WorkspaceActivated {
        id: u64,
        /// Whether this workspace also became focused. A workspace can become
        /// active on its output without taking focus, and only the focused one
        /// decides which workspace the user is looking at.
        focused: bool,
    },
    WindowOpenedOrChanged {
        id: u64,
        app_id: Option<String>,
        title: Option<String>,
        floating: bool,
    },
    WindowClosed {
        id: u64,
    },
    /// niri reloaded its configuration, so nsticky re-reads its own too.
    ConfigLoaded {
        failed: bool,
    },
    /// The workspace list changed: workspaces were added, removed, renamed or
    /// moved. Also arrives as the initial snapshot when the stream opens.
    ///
    /// The payload is not kept: a handler asks the compositor for the current
    /// layout, since a payload can be stale by the time it is handled.
    WorkspacesChanged,
    /// The overview opened or closed. While it is open the user is looking at
    /// every workspace, so nsticky must not shuffle the strip underneath them.
    OverviewChanged {
        open: bool,
    },
}

fn parse_niri_event(v: &Value) -> Option<NiriEvent> {
    if let Some(ws) = v.get("WorkspaceActivated") {
        let id = ws.get("id")?.as_u64()?;
        // A compositor without the flag counts as focused, as before it existed.
        let focused = ws.get("focused").and_then(|v| v.as_bool()).unwrap_or(true);
        return Some(NiriEvent::WorkspaceActivated { id, focused });
    }

    if let Some(window_event) = v.get("WindowOpenedOrChanged") {
        let window = window_event.get("window")?;
        let id = window.get("id")?.as_u64()?;
        let app_id = window
            .get("app_id")
            .and_then(|v| v.as_str())
            .map(String::from);
        let title = window
            .get("title")
            .and_then(|v| v.as_str())
            .map(String::from);
        let floating = window
            .get("is_floating")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        return Some(NiriEvent::WindowOpenedOrChanged {
            id,
            app_id,
            title,
            floating,
        });
    }

    if v.get("WorkspacesChanged").is_some() {
        return Some(NiriEvent::WorkspacesChanged);
    }

    if let Some(overview) = v.get("OverviewOpenedOrClosed") {
        let open = overview
            .get("is_open")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        return Some(NiriEvent::OverviewChanged { open });
    }

    if let Some(loaded) = v.get("ConfigLoaded") {
        let failed = loaded
            .get("failed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        return Some(NiriEvent::ConfigLoaded { failed });
    }

    if let Some(closed) = v.get("WindowClosed") {
        let id = closed.get("id")?.as_u64()?;
        return Some(NiriEvent::WindowClosed { id });
    }

    None
}

/// Stream of Niri events, connected with the `EventStream` IPC request.
#[derive(Debug)]
pub struct NiriEventStream {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
}

impl NiriEventStream {
    /// Next event nsticky cares about, or `None` once Niri closes the stream.
    /// Unrelated events are skipped.
    pub async fn next_event(&mut self) -> Result<Option<NiriEvent>> {
        let mut line = String::new();
        loop {
            let bytes_read = self.reader.read_line(&mut line).await?;
            if bytes_read == 0 {
                return Ok(None);
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                line.clear();
                continue;
            }

            if let Ok(value) = serde_json::from_str::<Value>(trimmed)
                && let Some(event) = parse_niri_event(&value)
            {
                return Ok(Some(event));
            }
            line.clear();
        }
    }
}

/// Open the Niri event stream. Requires `NIRI_SOCKET`.
pub async fn get_event_stream() -> Result<NiriEventStream> {
    let socket_path = std::env::var("NIRI_SOCKET").context("NIRI_SOCKET env var not set")?;
    open_event_stream(Path::new(&socket_path)).await
}

/// Open the event stream on one socket. The path is an argument so tests can
/// point it at a stub.
async fn open_event_stream(socket_path: &Path) -> Result<NiriEventStream> {
    let stream = UnixStream::connect(socket_path).await?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    let cmd_str = serde_json::to_string(&serde_json::json!("EventStream"))? + "\n";
    writer.write_all(cmd_str.as_bytes()).await?;
    writer.flush().await?;
    drop(writer);

    // The request is answered before events flow, so the handshake reply has
    // to be read here: a refusal must end the open with an error instead of
    // leaving the stream silently blocked waiting for events that will never
    // come (the reply used to be just one more line the parser skipped).
    let mut reply = String::new();
    reader.read_line(&mut reply).await?;
    if reply.trim().is_empty() {
        bail!("Niri closed the connection before answering the EventStream request");
    }
    let value: Value = serde_json::from_str(reply.trim())
        .with_context(|| format!("Unexpected EventStream handshake reply: {reply:?}"))?;
    if value.get("Ok").is_some() {
        return Ok(NiriEventStream { reader });
    }
    if value.get("Err").is_some() {
        bail!(
            "Niri refused the EventStream request: {}",
            value["Err"].as_str().unwrap_or("unknown error")
        );
    }
    bail!("Unexpected EventStream handshake reply: {value}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;

    /// Compositor stand-in for the event stream: answers with `lines`, closes,
    /// and records what the client sent.
    struct Stub {
        path: PathBuf,
        received: Arc<Mutex<Vec<String>>>,
    }

    impl Stub {
        async fn start(lines: Vec<String>) -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);

            let path = std::env::temp_dir().join(format!(
                "nsticky-events-test-{}-{}.sock",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let received = Arc::new(Mutex::new(Vec::new()));
            let log = Arc::clone(&received);

            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let (reader, mut writer) = stream.into_split();
                    let mut reader = BufReader::new(reader);
                    let log = Arc::clone(&log);
                    let lines = lines.clone();
                    tokio::spawn(async move {
                        let mut request = String::new();
                        if reader.read_line(&mut request).await.unwrap_or(0) > 0 {
                            log.lock().await.push(request.trim_end().to_string());
                        }
                        for line in lines {
                            if writer
                                .write_all(format!("{line}\n").as_bytes())
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        let _ = writer.flush().await;
                    });
                }
            });

            Self { path, received }
        }

        fn path(&self) -> PathBuf {
            self.path.clone()
        }

        async fn received(&self) -> Vec<String> {
            self.received.lock().await.clone()
        }
    }

    impl Drop for Stub {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn test_parse_workspace_activated() {
        let event =
            parse_niri_event(&json!({ "WorkspaceActivated": { "id": 3, "focused": true } }));
        assert!(matches!(
            event,
            Some(NiriEvent::WorkspaceActivated {
                id: 3,
                focused: true
            })
        ));

        let event =
            parse_niri_event(&json!({ "WorkspaceActivated": { "id": 4, "focused": false } }));
        assert!(matches!(
            event,
            Some(NiriEvent::WorkspaceActivated {
                id: 4,
                focused: false
            })
        ));

        // Without the flag, the activation counts as focused, as before it existed.
        let event = parse_niri_event(&json!({ "WorkspaceActivated": { "id": 5 } }));
        assert!(matches!(
            event,
            Some(NiriEvent::WorkspaceActivated {
                id: 5,
                focused: true
            })
        ));
    }

    #[test]
    fn test_parse_overview_opened_or_closed() {
        assert!(matches!(
            parse_niri_event(&json!({ "OverviewOpenedOrClosed": { "is_open": true } })),
            Some(NiriEvent::OverviewChanged { open: true })
        ));
        assert!(matches!(
            parse_niri_event(&json!({ "OverviewOpenedOrClosed": { "is_open": false } })),
            Some(NiriEvent::OverviewChanged { open: false })
        ));
        // A payload without the flag counts as closed: parking stays deferred
        // only while the overview is positively open.
        assert!(matches!(
            parse_niri_event(&json!({ "OverviewOpenedOrClosed": {} })),
            Some(NiriEvent::OverviewChanged { open: false })
        ));
    }

    #[test]
    fn test_parse_window_opened_or_changed() {
        let event = parse_niri_event(&json!({
            "WindowOpenedOrChanged": {
                "window": { "id": 7, "app_id": "firefox", "title": "Inbox" }
            }
        }));
        match event {
            Some(NiriEvent::WindowOpenedOrChanged {
                id,
                app_id,
                title,
                floating,
            }) => {
                assert_eq!(id, 7);
                assert_eq!(app_id.as_deref(), Some("firefox"));
                assert_eq!(title.as_deref(), Some("Inbox"));
                assert!(!floating, "is_floating was absent");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let event = parse_niri_event(&json!({
            "WindowOpenedOrChanged": {
                "window": { "id": 8, "app_id": "mpv", "is_floating": true }
            }
        }));
        match event {
            Some(NiriEvent::WindowOpenedOrChanged { floating, .. }) => assert!(floating),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn test_parse_window_closed() {
        let event = parse_niri_event(&json!({ "WindowClosed": { "id": 9 } }));
        assert!(matches!(event, Some(NiriEvent::WindowClosed { id: 9 })));
    }

    #[test]
    fn test_parse_workspaces_changed() {
        // The payload is ignored: nsticky asks for the current layout instead.
        let event = parse_niri_event(&json!({
            "WorkspacesChanged": {
                "workspaces": [
                    {"id": 1, "idx": 1, "name": null, "output": "DP-1", "is_active": true}
                ]
            }
        }));

        assert!(matches!(event, Some(NiriEvent::WorkspacesChanged)));
    }

    #[test]
    fn test_parse_config_loaded() {
        let event = parse_niri_event(&json!({ "ConfigLoaded": { "failed": true } }));
        assert!(matches!(
            event,
            Some(NiriEvent::ConfigLoaded { failed: true })
        ));
    }

    #[test]
    fn test_parse_ignores_unrelated_events() {
        assert!(parse_niri_event(&json!({ "WorkspaceActiveWindowChanged": {} })).is_none());
        assert!(parse_niri_event(&json!({})).is_none());
        // Malformed payloads for a known event are ignored, not fatal.
        assert!(parse_niri_event(&json!({ "WorkspaceActivated": {} })).is_none());
    }

    #[test]
    fn test_parse_tolerates_unknown_events_and_fields() {
        // niri adds events and fields over time; neither may stop the stream.
        assert!(parse_niri_event(&json!({ "SomeFutureEvent": { "x": 1 } })).is_none());
        assert!(matches!(
            parse_niri_event(&json!({ "ConfigLoaded": { "failed": null, "extra": 2 } })),
            Some(NiriEvent::ConfigLoaded { failed: false })
        ));
        assert!(matches!(
            parse_niri_event(&json!({ "ConfigLoaded": {} })),
            Some(NiriEvent::ConfigLoaded { failed: false })
        ));

        // A known event whose payload cannot be read is skipped, not fatal.
        assert!(parse_niri_event(&json!({ "WindowClosed": {} })).is_none());
        assert!(parse_niri_event(&json!({ "WindowClosed": { "id": "9" } })).is_none());
        assert!(parse_niri_event(&json!({ "WindowOpenedOrChanged": { "window": {} } })).is_none());
        assert!(
            parse_niri_event(&json!({ "WindowOpenedOrChanged": { "window": { "id": "7" } } }))
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_the_stream_skips_events_nsticky_does_not_use() {
        let stub = Stub::start(vec![
            r#"{"Ok":"Handled"}"#.to_string(),
            r#"{"KeyboardLayoutsChanged":{"keyboard_layouts":{"names":["us"]}}}"#.to_string(),
            r#"{"OverviewOpenedOrClosed":{"is_open":false}}"#.to_string(),
            r#"{"WindowFocusChanged":{"id":12}}"#.to_string(),
            r#"{"WorkspaceActivated":{"id":3,"focused":true,"is_urgent":false}}"#.to_string(),
        ])
        .await;

        let mut stream = open_event_stream(&stub.path()).await.unwrap();
        // The overview event is used; the layout and focus events are skipped.
        assert!(matches!(
            stream.next_event().await.unwrap(),
            Some(NiriEvent::OverviewChanged { open: false })
        ));
        assert!(matches!(
            stream.next_event().await.unwrap(),
            Some(NiriEvent::WorkspaceActivated {
                id: 3,
                focused: true
            })
        ));
        // The compositor closing the stream ends it, without an error.
        assert!(stream.next_event().await.unwrap().is_none());
        assert_eq!(stub.received().await, vec![r#""EventStream""#]);
    }

    #[tokio::test]
    async fn test_next_event_skips_lines_it_cannot_parse() {
        let stub = Stub::start(vec![
            r#"{"Ok":"Handled"}"#.to_string(),
            String::new(),
            "not json at all".to_string(),
            r#"{"WorkspaceActivated":{}}"#.to_string(),
            r#"{"WindowOpenedOrChanged":{}}"#.to_string(),
            r#"{"WindowOpenedOrChanged":{"window":{}}}"#.to_string(),
            r#"{"WindowClosed":{"id":"9"}}"#.to_string(),
            r#"{"WindowClosed":{"id":9}}"#.to_string(),
        ])
        .await;

        let mut stream = open_event_stream(&stub.path()).await.unwrap();
        assert!(matches!(
            stream.next_event().await.unwrap(),
            Some(NiriEvent::WindowClosed { id: 9 })
        ));
        assert!(stream.next_event().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_window_opened_or_changed_tolerates_null_fields() {
        let stub = Stub::start(vec![
            r#"{"Ok":"Handled"}"#.to_string(),
            r#"{"WindowOpenedOrChanged":{"window":{"id":7,"app_id":null,"title":null,"workspace_id":null,"is_floating":null}}}"#
                .to_string(),
        ])
        .await;

        let mut stream = open_event_stream(&stub.path()).await.unwrap();
        match stream.next_event().await.unwrap() {
            Some(NiriEvent::WindowOpenedOrChanged {
                id,
                app_id,
                title,
                floating,
            }) => {
                assert_eq!(id, 7);
                assert_eq!(app_id, None);
                assert_eq!(title, None);
                assert!(!floating, "a null is_floating means not floating");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_config_loaded_and_workspaces_changed_arrive_through_the_stream() {
        let stub = Stub::start(vec![
            r#"{"Ok":"Handled"}"#.to_string(),
            r#"{"ConfigLoaded":{"failed":"yes"}}"#.to_string(),
            r#"{"ConfigLoaded":{"failed":true}}"#.to_string(),
            r#"{"WorkspacesChanged":{"workspaces":[{"id":1,"idx":1,"name":null,"output":"DP-1","is_active":true}]}}"#
                .to_string(),
            r#"{"WorkspaceActivated":{"id":1}}"#.to_string(),
        ])
        .await;

        let mut stream = open_event_stream(&stub.path()).await.unwrap();
        assert!(matches!(
            stream.next_event().await.unwrap(),
            Some(NiriEvent::ConfigLoaded { failed: false })
        ));
        assert!(matches!(
            stream.next_event().await.unwrap(),
            Some(NiriEvent::ConfigLoaded { failed: true })
        ));
        assert!(matches!(
            stream.next_event().await.unwrap(),
            Some(NiriEvent::WorkspacesChanged)
        ));
        // No `focused` field counts as focused, as before that field existed.
        assert!(matches!(
            stream.next_event().await.unwrap(),
            Some(NiriEvent::WorkspaceActivated {
                id: 1,
                focused: true
            })
        ));
    }

    #[tokio::test]
    async fn test_a_refused_handshake_ends_the_open_with_an_error() {
        // The `Err` reply is read at open: the stream is never left blocked on
        // events that a compositor which refused the request will not send.
        let stub = Stub::start(vec![r#"{"Err":"unknown request"}"#.to_string()]).await;

        let error = open_event_stream(&stub.path()).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("Niri refused the EventStream request"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn test_an_unreadable_handshake_ends_the_open_with_an_error() {
        let stub = Stub::start(vec!["not a handshake".to_string()]).await;

        let error = open_event_stream(&stub.path()).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("Unexpected EventStream handshake"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn test_a_connection_closed_before_the_handshake_ends_the_open_with_an_error() {
        // One line, read as a (broken) handshake... the connection then closes,
        // which surfaces as a failed open, not a silent dead stream.
        let stub = Stub::start(vec![]).await;

        let error = open_event_stream(&stub.path()).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("closed the connection"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn test_opening_a_stream_on_a_missing_socket_fails() {
        let path =
            std::env::temp_dir().join(format!("nsticky-no-events-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);

        assert!(open_event_stream(&path).await.is_err());
    }
}
