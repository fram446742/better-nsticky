//! JSON IPC client for Niri's `$NIRI_SOCKET`.
//!
//! Requests share a connection: niri answers them in order on one connection,
//! which is what `niri msg` relies on, so a workspace switch (one query plus one
//! move per sticky window) costs one connection instead of one per call. The
//! connection is re-established when it breaks.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    net::unix::{OwnedReadHalf, OwnedWriteHalf},
    sync::Mutex,
    time::{Duration, timeout},
};

use super::{Niri, NiriFuture, Size, WindowInfo, WorkspaceInfo};

const IPC_TIMEOUT: Duration = Duration::from_secs(5);

/// An open request/response connection to the compositor.
#[derive(Debug)]
struct Connection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Connection {
    async fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .await
            .with_context(|| format!("Failed to connect to Niri at {path:?}"))?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
        })
    }

    async fn write_request(&mut self, request: &Value) -> Result<()> {
        let line = serde_json::to_string(request)? + "\n";
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Read one reply. `None` means the compositor closed the connection before
    /// answering, which is not the same as a broken reply.
    async fn read_reply(&mut self) -> Result<Option<Value>> {
        let mut response = String::new();
        if self.reader.read_line(&mut response).await? == 0 {
            return Ok(None);
        }

        let reply: Value = serde_json::from_str(response.trim())?;
        if let Some(err) = reply.get("Err") {
            bail!("Niri IPC error: {err}");
        }
        match reply.get("Ok") {
            Some(payload) => Ok(Some(payload.clone())),
            None => bail!("Unexpected Niri reply format: {response}"),
        }
    }
}

/// Why a single attempt at request/response failed.
enum AttemptError {
    /// No reply arrived: the request may never have been applied.
    NoReply(anyhow::Error),
    /// The compositor answered with an error, or the reply was unreadable: the
    /// request may well have been applied.
    Failed(anyhow::Error),
}

async fn send_and_receive(
    connection: &mut Connection,
    request: &Value,
) -> Result<Value, AttemptError> {
    if let Err(e) = connection.write_request(request).await {
        return Err(AttemptError::NoReply(
            e.context("Failed to send the request to Niri"),
        ));
    }

    match connection.read_reply().await {
        Ok(Some(reply)) => Ok(reply),
        Ok(None) => Err(AttemptError::NoReply(anyhow!(
            "Niri closed the connection without answering"
        ))),
        Err(e) => Err(AttemptError::Failed(e)),
    }
}

/// Niri client whose requests share one connection.
#[derive(Debug)]
pub struct NiriClient {
    socket_path: PathBuf,
    connection: Mutex<Option<Connection>>,
    /// Deadline for one request, so a compositor that opens a connection and
    /// then stays silent cannot hang the daemon.
    ipc_timeout: Duration,
}

impl NiriClient {
    /// Client for a specific compositor socket.
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            connection: Mutex::new(None),
            ipc_timeout: IPC_TIMEOUT,
        }
    }

    /// Client for the compositor named by `$NIRI_SOCKET`.
    pub fn from_env() -> Result<Self> {
        Self::from_socket_env(std::env::var("NIRI_SOCKET").ok())
    }

    fn from_socket_env(socket_path: Option<String>) -> Result<Self> {
        let socket_path = socket_path.context("NIRI_SOCKET env var not set")?;
        Ok(Self::new(PathBuf::from(socket_path)))
    }

    /// Send one request and return its `Ok` payload.
    async fn request(&self, request: &Value) -> Result<Value> {
        timeout(self.ipc_timeout, self.request_session(request))
            .await
            .context("Niri IPC timeout")?
    }

    async fn request_session(&self, request: &Value) -> Result<Value> {
        let mut connection = self.connection.lock().await;
        let mut attempt = 0;

        loop {
            if connection.is_none() {
                *connection = Some(Connection::connect(&self.socket_path).await?);
            }

            let open = connection.as_mut().expect("just connected");
            match send_and_receive(open, request).await {
                Ok(reply) => return Ok(reply),

                // No reply arrived: the compositor may have restarted and closed
                // the connection, and every action nsticky sends is an idempotent
                // window move, so retry once on a fresh connection.
                Err(AttemptError::NoReply(e)) => {
                    *connection = None;
                    attempt += 1;
                    if attempt >= 2 {
                        return Err(e.context("Niri did not answer"));
                    }
                }

                // The compositor reached a verdict (or the reply was broken):
                // report it instead of risking a second application.
                Err(AttemptError::Failed(e)) => return Err(e),
            }
        }
    }
}

impl Niri for NiriClient {
    fn windows(&self) -> NiriFuture<'_, Vec<WindowInfo>> {
        Box::pin(async move { parse_windows(&self.request(&json!("Windows")).await?) })
    }

    fn workspaces(&self) -> NiriFuture<'_, Vec<WorkspaceInfo>> {
        Box::pin(async move { parse_workspaces(&self.request(&json!("Workspaces")).await?) })
    }

    fn active_window(&self) -> NiriFuture<'_, u64> {
        Box::pin(async move { active_window_id(&self.request(&json!("FocusedWindow")).await?) })
    }

    fn active_workspace(&self) -> NiriFuture<'_, u64> {
        Box::pin(async move { active_workspace_id(&self.request(&json!("Workspaces")).await?) })
    }

    fn move_window<'a>(&'a self, window_id: u64, workspace_id: u64) -> NiriFuture<'a, ()> {
        Box::pin(async move {
            let action = build_move_window_action(window_id, workspace_id);
            self.request(&action).await?;
            Ok(())
        })
    }

    fn name_workspace<'a>(&'a self, name: &'a str, workspace_id: u64) -> NiriFuture<'a, ()> {
        Box::pin(async move {
            let action = json!({
                "Action": {
                    "SetWorkspaceName": {
                        "name": name,
                        "workspace": { "Id": workspace_id },
                    }
                }
            });
            self.request(&action).await?;
            Ok(())
        })
    }

    fn spawn<'a>(&'a self, command: &'a [String]) -> NiriFuture<'a, ()> {
        Box::pin(async move {
            let action = json!({ "Action": { "Spawn": { "command": command } } });
            self.request(&action).await?;
            Ok(())
        })
    }

    fn focus_window(&self, window_id: u64) -> NiriFuture<'_, ()> {
        Box::pin(async move {
            let action = json!({ "Action": { "FocusWindow": { "id": window_id } } });
            self.request(&action).await?;
            Ok(())
        })
    }

    fn float_window(&self, window_id: u64) -> NiriFuture<'_, ()> {
        Box::pin(async move {
            let action = json!({ "Action": { "MoveWindowToFloating": { "id": window_id } } });
            self.request(&action).await?;
            Ok(())
        })
    }

    fn move_floating_window(&self, window_id: u64, x: f64, y: f64) -> NiriFuture<'_, ()> {
        Box::pin(async move {
            let action = json!({
                "Action": {
                    "MoveFloatingWindow": {
                        "id": window_id,
                        "x": { "SetFixed": x },
                        "y": { "SetFixed": y },
                    }
                }
            });
            self.request(&action).await?;
            Ok(())
        })
    }

    fn move_workspace_to_index(&self, workspace_id: u64, index: usize) -> NiriFuture<'_, ()> {
        Box::pin(async move {
            let action = json!({
                "Action": {
                    "MoveWorkspaceToIndex": {
                        "index": index,
                        "reference": { "Id": workspace_id },
                    }
                }
            });
            self.request(&action).await?;
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
            for (action_name, size) in [("SetWindowWidth", width), ("SetWindowHeight", height)] {
                let Some(size) = size else {
                    continue;
                };
                let change = match size {
                    Size::Fixed(pixels) => json!({ "SetFixed": pixels }),
                    Size::Percent(percent) => json!({ "SetProportion": percent }),
                };
                let action = json!({
                    "Action": {
                        action_name: { "id": window_id, "change": change }
                    }
                });
                self.request(&action).await?;
            }
            Ok(())
        })
    }

    fn unname_workspace<'a>(&'a self, workspace_id: u64) -> NiriFuture<'a, ()> {
        Box::pin(async move {
            let action = json!({
                "Action": {
                    "UnsetWorkspaceName": {
                        "reference": { "Id": workspace_id },
                    }
                }
            });
            self.request(&action).await?;
            Ok(())
        })
    }
}

fn parse_windows(payload: &Value) -> Result<Vec<WindowInfo>> {
    let items = payload
        .get("Windows")
        .and_then(|windows| windows.as_array())
        .context("Windows field not found or not an array")?;

    Ok(items
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_u64()?;
            Some(WindowInfo {
                id,
                app_id: item
                    .get("app_id")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                title: item.get("title").and_then(|v| v.as_str()).map(String::from),
                workspace_id: item.get("workspace_id").and_then(|v| v.as_u64()),
                floating: item.get("is_floating").and_then(|v| v.as_bool()) == Some(true),
                size: window_size(item),
                position: window_position(item),
            })
        })
        .collect())
}

/// Size niri reports for a window (`layout.window_size`, logical pixels).
fn window_size(item: &Value) -> Option<(i32, i32)> {
    let size = item.get("layout")?.get("window_size")?.as_array()?;
    Some((
        size.first()?.as_f64()? as i32,
        size.get(1)?.as_f64()? as i32,
    ))
}

/// Position of a window on screen (`layout.tile_pos_in_workspace_view`).
fn window_position(item: &Value) -> Option<(f64, f64)> {
    let position = item
        .get("layout")?
        .get("tile_pos_in_workspace_view")?
        .as_array()?;
    Some((position.first()?.as_f64()?, position.get(1)?.as_f64()?))
}

pub(super) fn parse_workspaces(payload: &Value) -> Result<Vec<WorkspaceInfo>> {
    let items = payload
        .get("Workspaces")
        .and_then(|workspaces| workspaces.as_array())
        .context("Workspaces field not found or not an array")?;

    Ok(items
        .iter()
        .filter_map(|workspace| {
            let id = workspace.get("id")?.as_u64()?;
            Some(WorkspaceInfo {
                id,
                idx: workspace.get("idx").and_then(|v| v.as_u64()).unwrap_or(0) as u8,
                name: workspace
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                output: workspace
                    .get("output")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                is_active: workspace.get("is_active").and_then(|v| v.as_bool()) == Some(true),
            })
        })
        .collect())
}

fn active_workspace_id(payload: &Value) -> Result<u64> {
    parse_workspaces(payload)?
        .into_iter()
        .find(|workspace| workspace.is_active)
        .map(|workspace| workspace.id)
        .context("Active workspace not found")
}

fn active_window_id(payload: &Value) -> Result<u64> {
    payload
        .get("FocusedWindow")
        .and_then(|window| window.get("id"))
        .and_then(|id| id.as_u64())
        .context("Focused window id not found")
}

fn build_move_window_action(win_id: u64, workspace_id: u64) -> Value {
    let reference = json!({ "Id": workspace_id });
    json!({
        "Action": {
            "MoveWindowToWorkspace": {
                "window_id": win_id,
                "focus": false,
                "reference": reference
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// What the stub does with one accepted connection.
    #[derive(Debug, Clone)]
    enum Behaviour {
        /// Send these lines, one per request, repeating the last one. The
        /// connection closes once `close_after` replies have been sent.
        Reply {
            lines: Vec<String>,
            close_after: Option<usize>,
        },
        /// Close as soon as a request arrives, without answering.
        CloseOnRequest,
        /// Close without answering and stop listening, like a compositor that
        /// died between two attempts.
        CloseAndStop,
        /// Read the request and then never answer.
        Silence,
    }

    /// Answer every request with this one line.
    fn answers(reply: &str) -> Behaviour {
        Behaviour::Reply {
            lines: vec![reply.to_string()],
            close_after: None,
        }
    }

    /// Stand-in for the compositor socket. One script entry per accepted
    /// connection (the last repeats), plus the raw requests it received, so a
    /// test can assert what the client sent and how often it reconnected.
    struct Stub {
        path: PathBuf,
        connections: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl Stub {
        async fn start(script: Vec<Behaviour>) -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);

            let path = std::env::temp_dir().join(format!(
                "nsticky-client-test-{}-{}.sock",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let connections = Arc::new(AtomicUsize::new(0));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let (counter, log) = (Arc::clone(&connections), Arc::clone(&requests));
            let listen_path = path.clone();

            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let index = counter.fetch_add(1, Ordering::SeqCst);
                    let behaviour = script[index.min(script.len() - 1)].clone();

                    if matches!(behaviour, Behaviour::CloseAndStop) {
                        // Stop listening before the client learns its connection
                        // is gone: otherwise a retry can still be accepted into
                        // the backlog, and what a dead compositor looks like
                        // depends on how the kernel unwinds that backlog.
                        drop(listener);
                        let _ = std::fs::remove_file(&listen_path);
                        serve(stream, behaviour, Arc::clone(&log)).await;
                        break;
                    }

                    tokio::spawn(serve(stream, behaviour, Arc::clone(&log)));
                }
            });

            Self {
                path,
                connections,
                requests,
            }
        }

        fn path(&self) -> PathBuf {
            self.path.clone()
        }

        fn connections(&self) -> usize {
            self.connections.load(Ordering::SeqCst)
        }

        /// Requests as JSON, in the order the stub read them.
        async fn requests(&self) -> Vec<Value> {
            self.raw_requests()
                .await
                .iter()
                .map(|line| serde_json::from_str(line).expect("a request must be JSON"))
                .collect()
        }

        /// Requests as the exact bytes the compositor will see, newline left
        /// out because the reader trims it.
        async fn raw_requests(&self) -> Vec<String> {
            self.requests.lock().await.clone()
        }
    }

    impl Drop for Stub {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Serve one connection according to the script.
    async fn serve(stream: UnixStream, behaviour: Behaviour, log: Arc<Mutex<Vec<String>>>) {
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut sent = 0;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                break;
            }
            log.lock().await.push(line.trim_end().to_string());

            match &behaviour {
                Behaviour::CloseOnRequest | Behaviour::CloseAndStop => break,
                Behaviour::Silence => std::future::pending::<()>().await,
                Behaviour::Reply { lines, close_after } => {
                    let reply = &lines[sent.min(lines.len() - 1)];
                    if writer
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .is_err()
                    {
                        break;
                    }
                    let _ = writer.flush().await;

                    sent += 1;
                    if *close_after == Some(sent) {
                        break;
                    }
                }
            }
        }
    }

    #[test]
    fn test_build_move_window_action_by_id() {
        let action = build_move_window_action(42, 7);
        let expected = json!({
            "Action": {
                "MoveWindowToWorkspace": {
                    "window_id": 42,
                    "focus": false,
                    "reference": { "Id": 7 }
                }
            }
        });
        assert_eq!(action, expected);
    }

    #[tokio::test]
    async fn test_actions_send_the_exact_json_the_compositor_sees() {
        let stub = Stub::start(vec![answers(r#"{"Ok":"Handled"}"#)]).await;
        let client = NiriClient::new(stub.path());

        client.move_window(42, 7).await.unwrap();
        client.name_workspace("stage", 7).await.unwrap();
        client.unname_workspace(7).await.unwrap();
        client
            .spawn(&["foot".to_string(), "-e".to_string(), "htop".to_string()])
            .await
            .unwrap();
        client.focus_window(11).await.unwrap();
        client.float_window(11).await.unwrap();
        client.move_floating_window(11, 100.0, 200.0).await.unwrap();
        client.move_workspace_to_index(7, 3).await.unwrap();
        client
            .resize_window(11, Some(Size::Fixed(1200)), Some(Size::Percent(60.0)))
            .await
            .unwrap();

        let expected = vec![
            json!({"Action": {"MoveWindowToWorkspace": {"window_id": 42, "focus": false, "reference": {"Id": 7}}}}),
            json!({"Action": {"SetWorkspaceName": {"name": "stage", "workspace": {"Id": 7}}}}),
            json!({"Action": {"UnsetWorkspaceName": {"reference": {"Id": 7}}}}),
            json!({"Action": {"Spawn": {"command": ["foot", "-e", "htop"]}}}),
            json!({"Action": {"FocusWindow": {"id": 11}}}),
            json!({"Action": {"MoveWindowToFloating": {"id": 11}}}),
            json!({"Action": {"MoveFloatingWindow": {"id": 11, "x": {"SetFixed": 100.0}, "y": {"SetFixed": 200.0}}}}),
            // By id, never by name or index: nsticky must not depend on the
            // workspace being declared in niri's configuration.
            json!({"Action": {"MoveWorkspaceToIndex": {"index": 3, "reference": {"Id": 7}}}}),
            json!({"Action": {"SetWindowWidth": {"id": 11, "change": {"SetFixed": 1200}}}}),
            json!({"Action": {"SetWindowHeight": {"id": 11, "change": {"SetProportion": 60.0}}}}),
        ];
        assert_eq!(stub.requests().await, expected);

        // One request per call, one JSON object per line: what niri reads.
        let raw = stub.raw_requests().await;
        assert_eq!(raw.len(), expected.len());
        assert_eq!(
            raw[0],
            r#"{"Action":{"MoveWindowToWorkspace":{"focus":false,"reference":{"Id":7},"window_id":42}}}"#
        );
        assert_eq!(stub.connections(), 1, "actions share one connection");
    }

    #[tokio::test]
    async fn test_resize_sends_only_the_sizes_it_was_given() {
        let stub = Stub::start(vec![answers(r#"{"Ok":"Handled"}"#)]).await;
        let client = NiriClient::new(stub.path());

        client.resize_window(11, None, None).await.unwrap();
        assert!(
            stub.raw_requests().await.is_empty(),
            "a size that was not given must not be sent as a default"
        );
        assert_eq!(stub.connections(), 0, "nothing to send, nothing to connect");

        client
            .resize_window(11, Some(Size::Fixed(900)), None)
            .await
            .unwrap();
        client
            .resize_window(11, None, Some(Size::Percent(50.0)))
            .await
            .unwrap();

        assert_eq!(
            stub.requests().await,
            vec![
                json!({"Action": {"SetWindowWidth": {"id": 11, "change": {"SetFixed": 900}}}}),
                json!({"Action": {"SetWindowHeight": {"id": 11, "change": {"SetProportion": 50.0}}}}),
            ]
        );
    }

    #[tokio::test]
    async fn test_queries_ask_for_what_they_parse() {
        let stub = Stub::start(vec![Behaviour::Reply {
            lines: vec![
                r#"{"Ok":{"Windows":[{"id":4,"app_id":"zen"}]}}"#.to_string(),
                r#"{"Ok":{"Workspaces":[{"id":3,"is_active":false},{"id":4,"is_active":true}]}}"#
                    .to_string(),
                r#"{"Ok":{"Workspaces":[{"id":3,"is_active":false},{"id":4,"is_active":true}]}}"#
                    .to_string(),
                r#"{"Ok":{"FocusedWindow":{"id":9}}}"#.to_string(),
            ],
            close_after: None,
        }])
        .await;
        let client = NiriClient::new(stub.path());

        let windows = client.windows().await.unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, 4);
        assert_eq!(client.workspaces().await.unwrap().len(), 2);
        assert_eq!(client.active_workspace().await.unwrap(), 4);
        assert_eq!(client.active_window().await.unwrap(), 9);

        assert_eq!(
            stub.raw_requests().await,
            vec![
                r#""Windows""#,
                r#""Workspaces""#,
                r#""Workspaces""#,
                r#""FocusedWindow""#,
            ]
        );
        assert_eq!(stub.connections(), 1, "queries share one connection too");
    }

    #[tokio::test]
    async fn test_a_null_payload_fails_in_the_parser_not_in_the_client() {
        let stub = Stub::start(vec![answers(r#"{"Ok":null}"#)]).await;
        let client = NiriClient::new(stub.path());

        let error = client.windows().await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Windows field not found or not an array"),
            "{error}"
        );
    }

    #[test]
    fn test_parse_windows_ignores_entries_without_an_id() {
        let payload = json!({"Windows": [
            {"id": 4, "app_id": "zen", "title": "Inbox", "workspace_id": 10},
            {"title": "no id"},
            {"id": 5, "app_id": null, "title": null, "workspace_id": null},
        ]});

        let windows = parse_windows(&payload).unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(
            windows[0],
            WindowInfo {
                id: 4,
                app_id: Some("zen".to_string()),
                title: Some("Inbox".to_string()),
                workspace_id: Some(10),
                floating: false,
                size: None,
                position: None,
            }
        );
        assert_eq!(windows[1].app_id, None);
        assert_eq!(windows[1].workspace_id, None);
    }

    #[test]
    fn test_parse_windows_reads_geometry() {
        let payload = json!({"Windows": [
            {"id": 4, "app_id": "foot", "is_floating": true,
             "layout": {"window_size": [1100.0, 480.0], "tile_pos_in_workspace_view": [730.0, 94.0]}},
        ]});

        let windows = parse_windows(&payload).unwrap();
        assert_eq!(windows[0].size, Some((1100, 480)));
        assert_eq!(windows[0].position, Some((730.0, 94.0)));
    }

    /// A window whose geometry cannot be read must end up with `None`, and a
    /// window whose `id` cannot be read must be dropped: inventing a size or an
    /// id is how a window manager starts moving the wrong window.
    #[test]
    fn test_parse_windows_keeps_unreadable_fields_at_none() {
        let payload = json!({"Windows": [
            {"id": 1},
            {"id": 2, "app_id": 7, "title": ["x"], "workspace_id": "9", "is_floating": "yes"},
            {"id": 3, "layout": "nope"},
            {"id": 4, "layout": {}},
            {"id": 5, "layout": {"window_size": "1100x480"}},
            {"id": 6, "layout": {"window_size": [1100]}},
            {"id": 7, "layout": {"window_size": [1100, "480"]}},
            {"id": 8, "layout": {"window_size": [1100, 480], "tile_pos_in_workspace_view": [1.0]}},
            {"id": 9, "layout": {"tile_pos_in_workspace_view": "1,2"}},
            {"id": 10, "app_id": "foot", "workspace_id": 4, "is_floating": true,
             "layout": {"window_size": [1100, 480], "tile_pos_in_workspace_view": [1.0, 2.0]}},
            {"id": "11", "app_id": "dropped"},
        ]});

        let windows = parse_windows(&payload).unwrap();
        assert_eq!(
            windows.len(),
            10,
            "a window without a numeric id is dropped"
        );

        for window in &windows[..7] {
            assert_eq!(window.size, None, "window {} invented a size", window.id);
            assert_eq!(
                window.position, None,
                "window {} invented a position",
                window.id
            );
            assert!(!window.floating, "window {} invented floating", window.id);
        }
        // A size that reads fine must survive a position that does not.
        assert_eq!(windows[7].size, Some((1100, 480)));
        assert_eq!(windows[7].position, None);
        for window in &windows[7..9] {
            assert_eq!(
                window.position, None,
                "window {} invented a position",
                window.id
            );
        }
        assert_eq!(windows[8].size, None);
        assert_eq!(windows[1].app_id, None, "a non-string app_id is not copied");
        assert_eq!(windows[1].title, None);
        assert_eq!(windows[1].workspace_id, None);
        assert_eq!(windows[1].size, None);

        // Integer sizes and positions are accepted like the floats niri sends.
        assert_eq!(windows[9].size, Some((1100, 480)));
        assert_eq!(windows[9].position, Some((1.0, 2.0)));
        assert!(windows[9].floating);
        assert_eq!(windows[9].workspace_id, Some(4));
    }

    #[test]
    fn test_parse_windows_rejects_an_unexpected_payload() {
        for payload in [
            json!({}),
            json!({"Windows": "nope"}),
            json!({"Windows": {}}),
            json!({"Windows": null}),
        ] {
            let error = parse_windows(&payload).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("Windows field not found or not an array"),
                "{payload} -> {error}"
            );
        }
    }

    #[test]
    fn test_parse_workspaces_rejects_an_unexpected_payload() {
        for payload in [
            json!({}),
            json!({"Workspaces": 3}),
            json!({"Workspaces": {}}),
        ] {
            let error = parse_workspaces(&payload).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("Workspaces field not found or not an array"),
                "{payload} -> {error}"
            );
        }
    }

    #[test]
    fn test_parse_workspaces_defaults_and_drops() {
        let payload = json!({"Workspaces": [
            {"id": 3, "idx": 9, "name": null, "output": null, "is_active": "yes"},
            {"id": 4, "idx": "3", "name": "stage", "output": "DP-1", "is_active": true},
            {"name": "no id"},
            {"id": "5", "name": "wrong id type"},
        ]});

        let workspaces = parse_workspaces(&payload).unwrap();
        assert_eq!(
            workspaces.len(),
            2,
            "a workspace without a numeric id is dropped"
        );
        assert_eq!(workspaces[0].idx, 9);
        assert_eq!(workspaces[0].name, None);
        assert_eq!(workspaces[0].output, None, "a null output stays none");
        assert!(!workspaces[0].is_active, "a non-boolean is_active is false");
        assert_eq!(workspaces[1].idx, 0, "an unreadable idx falls back to 0");
        assert_eq!(workspaces[1].name.as_deref(), Some("stage"));
        assert_eq!(workspaces[1].output.as_deref(), Some("DP-1"));
    }

    #[test]
    fn test_parse_workspaces_and_active_one() {
        let payload = json!({"Workspaces": [
            {"id": 3, "name": null, "is_active": false},
            {"id": 4, "name": "main", "is_active": true},
        ]});

        let workspaces = parse_workspaces(&payload).unwrap();
        assert_eq!(workspaces.len(), 2);
        assert_eq!(workspaces[1].name.as_deref(), Some("main"));
        assert!(workspaces[1].is_active);
        assert_eq!(active_workspace_id(&payload).unwrap(), 4);
    }

    #[test]
    fn test_active_workspace_requires_an_active_workspace() {
        let payload = json!({"Workspaces": [{"id": 3, "is_active": false}]});
        let error = active_workspace_id(&payload).unwrap_err();
        assert!(
            error.to_string().contains("Active workspace not found"),
            "{error}"
        );

        // The active workspace without an id is dropped, so there is none left.
        assert!(active_workspace_id(&json!({"Workspaces": [{"is_active": true}]})).is_err());
        assert!(active_workspace_id(&json!({})).is_err());
    }

    #[test]
    fn test_active_window_id() {
        assert_eq!(
            active_window_id(&json!({"FocusedWindow": {"id": 7}})).unwrap(),
            7
        );
        // No focused window (niri reports null) is an error, not a panic.
        assert!(active_window_id(&json!({"FocusedWindow": null})).is_err());
        assert!(active_window_id(&json!({})).is_err());
        assert!(active_window_id(&json!({"FocusedWindow": {"id": "7"}})).is_err());
        assert!(active_window_id(&json!({"FocusedWindow": {"id": -1}})).is_err());
    }

    #[test]
    fn test_missing_socket_env_is_reported() {
        let error = NiriClient::from_socket_env(None).unwrap_err();
        assert!(
            error.to_string().contains("NIRI_SOCKET env var not set"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn test_a_client_built_from_a_socket_value_talks_to_that_socket() {
        let stub = Stub::start(vec![answers(r#"{"Ok":{"Workspaces":[]}}"#)]).await;
        let socket = stub.path().display().to_string();
        let client = NiriClient::from_socket_env(Some(socket)).unwrap();

        assert!(client.workspaces().await.unwrap().is_empty());
        assert_eq!(stub.raw_requests().await, vec![r#""Workspaces""#]);
    }

    #[tokio::test]
    async fn test_requests_reuse_one_connection() {
        let stub = Stub::start(vec![answers(r#"{"Ok":{"Windows":[]}}"#)]).await;
        let client = NiriClient::new(stub.path());

        for _ in 0..3 {
            client.windows().await.unwrap();
        }

        assert_eq!(stub.connections(), 1, "one connection for three requests");
    }

    #[tokio::test]
    async fn test_client_reconnects_after_the_compositor_closes_the_connection() {
        // The stub closes after the first reply, so the second request goes
        // into a dying socket and must be retried on a fresh connection.
        let stub = Stub::start(vec![Behaviour::Reply {
            lines: vec![r#"{"Ok":{"Windows":[]}}"#.to_string()],
            close_after: Some(1),
        }])
        .await;
        let client = NiriClient::new(stub.path());

        client.windows().await.unwrap();
        client.windows().await.unwrap();

        assert_eq!(stub.connections(), 2, "the second request reconnected");
    }

    #[tokio::test]
    async fn test_a_lost_reply_is_retried_on_a_fresh_connection() {
        let stub = Stub::start(vec![
            Behaviour::CloseOnRequest,
            answers(r#"{"Ok":{"Windows":[]}}"#),
        ])
        .await;
        let client = NiriClient::new(stub.path());

        assert!(client.windows().await.unwrap().is_empty());
        assert_eq!(stub.connections(), 2);
        assert_eq!(
            stub.raw_requests().await,
            vec![r#""Windows""#, r#""Windows""#],
            "the retry repeats the request that was lost"
        );
    }

    #[tokio::test]
    async fn test_errors_from_the_compositor_are_reported() {
        let stub = Stub::start(vec![answers(r#"{"Err":"no such window"}"#)]).await;
        let client = NiriClient::new(stub.path());

        let error = client.windows().await.unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("Niri IPC error"), "{message}");
        assert!(message.contains("no such window"), "{message}");
    }

    #[tokio::test]
    async fn test_a_compositor_that_never_answers_is_an_error_after_one_retry() {
        let stub = Stub::start(vec![Behaviour::CloseOnRequest]).await;
        let client = NiriClient::new(stub.path());

        let error = client.windows().await.unwrap_err();
        assert!(
            format!("{error:#}").contains("Niri did not answer"),
            "{error:#}"
        );
        assert_eq!(
            stub.connections(),
            2,
            "one attempt plus one retry, then give up"
        );
    }

    #[tokio::test]
    async fn test_compositor_errors_are_not_retried() {
        let stub = Stub::start(vec![answers(r#"{"Err":"no such window"}"#)]).await;
        let client = NiriClient::new(stub.path());

        assert!(client.windows().await.is_err());
        assert_eq!(
            stub.connections(),
            1,
            "a verdict from the compositor is final"
        );
    }

    #[tokio::test]
    async fn test_a_reply_that_is_not_json_is_an_error_not_a_lost_one() {
        for raw in ["this is not json", "", "{"] {
            let stub = Stub::start(vec![answers(raw)]).await;
            let client = NiriClient::new(stub.path());

            let error = client.windows().await.unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("line 1"), "{raw:?} -> {message}");
            assert!(
                !message.contains("did not answer"),
                "{raw:?} must not look like a lost reply: {message}"
            );
            assert_eq!(stub.connections(), 1, "{raw:?} must not be retried");
        }
    }

    #[tokio::test]
    async fn test_a_reply_that_is_neither_ok_nor_err_is_rejected() {
        for raw in [r#"{"SomethingElse": 1}"#, "[1, 2]", r#""handled""#] {
            let stub = Stub::start(vec![answers(raw)]).await;
            let client = NiriClient::new(stub.path());

            let error = client.windows().await.unwrap_err();
            let message = error.to_string();
            assert!(
                message.contains("Unexpected Niri reply format"),
                "{raw} -> {message}"
            );
            assert!(message.contains(raw), "the odd reply is quoted: {message}");
            assert_eq!(stub.connections(), 1, "{raw} must not be retried");
        }
    }

    #[tokio::test]
    async fn test_a_compositor_that_dies_between_attempts_is_a_connection_error() {
        let stub = Stub::start(vec![Behaviour::CloseAndStop]).await;
        let client = NiriClient::new(stub.path());

        // The first attempt loses its request, and by the retry no listener is
        // left to accept the connection.
        let error = client.windows().await.unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("Failed to connect to Niri"),
            "{message} (accepted connections: {})",
            stub.connections()
        );
        assert!(!message.contains("did not answer"), "{message}");
        assert_eq!(stub.connections(), 1);
    }

    #[tokio::test]
    async fn test_a_silent_compositor_hits_the_request_timeout() {
        let stub = Stub::start(vec![Behaviour::Silence]).await;
        let mut client = NiriClient::new(stub.path());
        client.ipc_timeout = Duration::from_millis(100);

        let error = client.windows().await.unwrap_err();
        assert!(
            format!("{error:#}").contains("Niri IPC timeout"),
            "{error:#}"
        );
        assert_eq!(
            stub.raw_requests().await,
            vec![r#""Windows""#],
            "the request reached the compositor, only the reply is missing"
        );
    }

    #[tokio::test]
    async fn test_missing_compositor_socket_is_reported() {
        let path =
            std::env::temp_dir().join(format!("nsticky-no-such-niri-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let client = NiriClient::new(path.clone());

        let error = client.windows().await.unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("Failed to connect to Niri"), "{message}");
        assert!(
            message.contains(path.to_str().unwrap()),
            "the failing socket must be named: {message}"
        );
    }
}
