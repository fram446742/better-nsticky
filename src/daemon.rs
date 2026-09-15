//! The daemon: a Unix-socket CLI server plus the Niri event loop.

use anyhow::{Context, Result, bail};
use std::future;
use std::future::Future;
use std::sync::Arc;

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

use crate::{
    business::BusinessLogic,
    config::Config,
    niri::{
        NiriClient,
        events::{NiriEvent, NiriEventStream, get_event_stream},
    },
    protocol::{self, cli_socket_path},
    state_store::FileStorage,
};

/// Re-read the configuration, logging the outcome either way.
pub async fn reload(business_logic: &BusinessLogic, reason: &str) -> Result<String> {
    match business_logic.reload_from_disk().await {
        Ok(message) => {
            tracing::info!("{message} ({reason})");
            Ok(message)
        }
        Err(e) => {
            tracing::warn!("Reload failed ({reason}): {e:#}");
            Err(e)
        }
    }
}

/// Reload the configuration whenever this process receives `SIGHUP`.
async fn watch_hangup(business_logic: BusinessLogic) {
    use tokio::signal::unix::{SignalKind, signal};

    let mut hangup = match signal(SignalKind::hangup()) {
        Ok(hangup) => hangup,
        Err(e) => {
            tracing::warn!("Cannot listen for SIGHUP: {e:#}");
            return;
        }
    };

    while hangup.recv().await.is_some() {
        // A failed reload is logged, never fatal: the daemon keeps the
        // configuration it is already running with.
        let _ = reload(&business_logic, "SIGHUP").await;
    }
}

/// What to do when another daemon already owns the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Takeover {
    /// Refuse to start, so an accidental second instance cannot hijack the
    /// socket of a running daemon.
    Refuse,
    /// Take the socket over (`nsticky --replace`).
    Replace,
}

/// Maximum size of a CLI request, so a client cannot make the daemon allocate
/// without bound.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Upper bound on how much of an oversized request is read and discarded before
/// the connection is dropped.
const MAX_DISCARDED_BYTES: usize = 1024 * 1024;

/// Start the daemon: bind the CLI socket and follow Niri events forever.
pub async fn start(takeover: Takeover) -> Result<()> {
    let config = Config::load_or_default();
    let state_path = crate::state_store::default_path();
    tracing::info!("State file: {}", state_path.display());
    let storage = FileStorage::new(state_path);
    // Reading the socket path once means a missing NIRI_SOCKET fails here, with
    // a clear message, instead of on every request.
    let niri = Arc::new(NiriClient::from_env()?);
    let business_logic = BusinessLogic::new(
        config,
        crate::config::Config::default_config_path(),
        niri,
        Arc::new(storage),
    );

    // Windows staged before this run are only known to the compositor.
    if let Err(e) = business_logic.reconcile().await {
        tracing::warn!("Failed to restore the previous state: {e:#}");
    }

    let socket_path = cli_socket_path();
    let listener = bind_cli_socket(&socket_path, takeover).await?;
    tracing::info!("Listening on {}", socket_path.display());

    // Only now that the socket is ours: a daemon that refuses to start must not
    // subscribe to the compositor's event stream.
    let events = get_event_stream().await;
    serve(listener, events, business_logic).await
}

/// Answer CLI requests and follow Niri events until the process ends.
///
/// The event stream is opened by the caller and handed over as a result: a
/// compositor that cannot be reached is logged and leaves the CLI working,
/// which is what a restart or `nsticky config check` needs.
async fn serve(
    listener: UnixListener,
    events: Result<NiriEventStream>,
    business_logic: BusinessLogic,
) -> Result<()> {
    let cli_business_logic = business_logic.clone();
    tokio::spawn(async move {
        if let Err(e) = accept_cli_connections(listener, cli_business_logic).await {
            tracing::error!("CLI server error: {e:?}");
        }
    });

    tokio::spawn(watch_hangup(business_logic.clone()));

    tokio::spawn(async move {
        match events {
            Ok(events) => {
                if let Err(e) = run_watcher(business_logic, events).await {
                    tracing::error!("Watcher error: {e:?}");
                }
            }
            Err(e) => tracing::error!("Cannot follow Niri events: {e:#}"),
        }
    });

    tracing::info!("nsticky daemon started.");
    future::pending::<()>().await;
    Ok(())
}

/// Bind the CLI socket, replacing a stale one but never hijacking a live daemon.
async fn bind_cli_socket(path: &std::path::Path, takeover: Takeover) -> Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create socket directory: {dir:?}"))?;
    }

    // A socket that accepts connections means a daemon is already running.
    if UnixStream::connect(path).await.is_ok() {
        match takeover {
            Takeover::Refuse => bail!(
                "Another nsticky daemon is already running on {path:?}. \
                 Restart that one, or use `nsticky --replace` to take the socket over."
            ),
            Takeover::Replace => tracing::warn!("Taking over the socket from a running daemon"),
        }
    }
    let _ = std::fs::remove_file(path);

    let listener =
        UnixListener::bind(path).with_context(|| format!("Failed to bind {}", path.display()))?;

    // Only the user running the daemon may talk to it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("Failed to restrict permissions on {path:?}"))?;
    }

    Ok(listener)
}

async fn accept_cli_connections(
    listener: UnixListener,
    business_logic: BusinessLogic,
) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let business_logic_clone = business_logic.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_cli_connection(stream, business_logic_clone).await {
                tracing::error!("CLI connection error: {e:?}");
            }
        });
    }
}

/// Read one line, keeping at most `max` bytes and discarding the rest up to a
/// hard limit. Returns the line and whether it was truncated.
///
/// Discarding (rather than stopping at the limit) keeps the socket in a state
/// where the error reply can still be delivered.
async fn read_bounded_line<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    max: usize,
) -> Result<(String, bool)> {
    let mut kept: Vec<u8> = Vec::new();
    let mut truncated = false;

    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            break;
        }

        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let available = newline.map_or(chunk.len(), |position| position + 1);
        let room = max.saturating_sub(kept.len());
        let keep = available.min(room);
        kept.extend_from_slice(&chunk[..keep]);
        if keep < available {
            truncated = true;
        }
        reader.consume(available);

        if newline.is_some() || kept.len() + (available - keep) > MAX_DISCARDED_BYTES {
            break;
        }
    }

    Ok((String::from_utf8_lossy(&kept).trim().to_string(), truncated))
}

async fn handle_cli_connection(stream: UnixStream, business_logic: BusinessLogic) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // One connection can carry several requests (a script may keep the socket
    // open); each is answered before the next is read, so replies arrive in
    // order.
    loop {
        // Bounded: an oversized request is answered with an error instead of
        // being buffered without limit.
        let (line, truncated) = read_bounded_line(&mut reader, MAX_REQUEST_BYTES).await?;
        if line.is_empty() && !truncated {
            return Ok(());
        }

        let response = if truncated {
            protocol::Response::Error {
                message: format!(
                    "Request too large: at most {MAX_REQUEST_BYTES} bytes are accepted"
                ),
            }
        } else {
            match serde_json::from_str::<protocol::Request>(&line) {
                Ok(request) => business_logic.handle_request(request).await,
                Err(e) => protocol::Response::Error {
                    message: format!("Failed to parse request: {e}"),
                },
            }
        };

        let response_str =
            serde_json::to_string(&response).context("Failed to serialize response")?;
        writer.write_all(response_str.as_bytes()).await?;
        writer.write_all(b"\n").await?;
    }
}

/// Where the watcher gets its events from, so a test can drive it without a
/// compositor connection.
trait EventSource {
    fn next_event(&mut self) -> impl Future<Output = Result<Option<NiriEvent>>> + Send;
}

impl EventSource for NiriEventStream {
    fn next_event(&mut self) -> impl Future<Output = Result<Option<NiriEvent>>> + Send {
        NiriEventStream::next_event(self)
    }
}

async fn run_watcher(business_logic: BusinessLogic, mut events: impl EventSource) -> Result<()> {
    while let Some(event) = events.next_event().await? {
        match event {
            NiriEvent::WorkspaceActivated { id: ws_id, focused } => {
                // A workspace can become active on its output without taking
                // focus (for example when a window is moved to it), and moving
                // sticky windows then would drag them away for no reason.
                if !focused {
                    tracing::debug!("Workspace {ws_id} became active without focus");
                    continue;
                }
                tracing::info!("Workspace switched to: {ws_id}");
                if let Err(e) = business_logic.handle_workspace_activation(ws_id).await {
                    tracing::error!("Failed to handle workspace activation: {e:?}");
                }
            }
            NiriEvent::WindowOpenedOrChanged {
                id,
                app_id,
                title,
                floating,
            } => {
                if let Err(e) = business_logic
                    .handle_window_opened_or_changed(id, app_id, title, floating)
                    .await
                {
                    tracing::error!("Failed to handle window opened or changed: {e:?}");
                }
            }
            NiriEvent::WorkspacesChanged => {
                // Workspaces come and go (a window on the empty tail creates
                // one): keep the parking areas where they belong, at the end.
                if let Err(e) = business_logic.ensure_parking_order().await {
                    tracing::debug!("Failed to reorder the parking workspaces: {e:#}");
                }
            }
            NiriEvent::ConfigLoaded { failed } => {
                if failed {
                    tracing::warn!("niri failed to load its configuration");
                }
                // niri reloads its config on demand; do the same, so both
                // configs move together.
                let _ = reload(&business_logic, "niri reloaded its configuration").await;
            }
            NiriEvent::WindowClosed { id: window_id } => {
                tracing::info!("Window closed: {window_id}");
                if let Err(e) = business_logic.handle_window_closed(window_id).await {
                    tracing::error!("Failed to handle window closed: {e:?}");
                }
            }
        }
    }

    tracing::warn!(
        "Niri closed the event stream: sticky windows will not follow workspaces until the daemon restarts"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::niri::fake::FakeNiri;
    use crate::state_store::MemoryStorage;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    static SOCKET_SEQ: AtomicUsize = AtomicUsize::new(0);

    /// A directory of its own: tests run in parallel, so they must not share a
    /// socket with each other or with the daemon of the session.
    fn work_dir() -> std::path::PathBuf {
        let seq = SOCKET_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("nsticky-daemon-test-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn socket_path() -> std::path::PathBuf {
        work_dir().join("cli.sock")
    }

    /// Removes the directory a test's artefacts lived in.
    fn cleanup(path: &std::path::Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    fn fake_niri() -> Arc<FakeNiri> {
        Arc::new(FakeNiri::with_windows(&[(1, "fake", "Fake")]))
    }

    /// Business logic whose configuration file is a temp file, plus that file:
    /// a reload in a test must never read the developer's own config.
    fn business_with_config(
        niri: &Arc<FakeNiri>,
        toml: &str,
    ) -> (BusinessLogic, std::path::PathBuf) {
        let config_path = work_dir().join("config.toml");
        std::fs::write(&config_path, toml).unwrap();
        let business = BusinessLogic::new(
            Config::from_toml(toml).unwrap(),
            config_path.clone(),
            niri.clone(),
            Arc::new(MemoryStorage::new()),
        );
        (business, config_path)
    }

    /// Business logic over a fake compositor. Its configuration path is only
    /// read by a reload, which the tests that care about it drive through
    /// [`business_with_config`].
    fn business() -> BusinessLogic {
        BusinessLogic::new(
            Config::default(),
            std::path::PathBuf::from("unused-in-this-test.toml"),
            fake_niri(),
            Arc::new(MemoryStorage::new()),
        )
    }

    async fn sticky_ids(business: &BusinessLogic) -> String {
        match business.handle_request(protocol::Request::List).await {
            protocol::Response::Data { data } => data,
            other => panic!("expected the sticky list, got {other:?}"),
        }
    }

    /// One connection, one request: everything the daemon sent back.
    async fn one_request(path: &std::path::Path, request: &str) -> String {
        let stream = UnixStream::connect(path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        writer.write_all(request.as_bytes()).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
        writer.flush().await.unwrap();
        // The daemon serves until the client says it is done.
        drop(writer);

        let mut reply = String::new();
        BufReader::new(reader)
            .read_to_string(&mut reply)
            .await
            .unwrap();
        reply
    }

    /// Sends raw bytes on one CLI connection and returns the replies, so a test
    /// sees exactly what a client would.
    async fn replies_to(request: &[u8]) -> Vec<String> {
        let (client, server) = UnixStream::pair().unwrap();
        let handler = tokio::spawn(handle_cli_connection(server, business()));

        let (reader, mut writer) = client.into_split();
        writer.write_all(request).await.unwrap();
        writer.flush().await.unwrap();
        drop(writer);

        let mut replies = Vec::new();
        let mut lines = BufReader::new(reader).lines();
        while let Some(line) = lines.next_line().await.unwrap() {
            replies.push(line);
        }

        handler.await.unwrap().unwrap();
        replies
    }

    fn socket_inode(path: &std::path::Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).map(|meta| meta.ino()).unwrap_or(0)
    }

    /// An event source a test can queue events into, so the watcher can be
    /// driven without a compositor.
    struct FakeEvents(VecDeque<Result<Option<NiriEvent>>>);

    impl FakeEvents {
        fn new(events: impl IntoIterator<Item = NiriEvent>) -> Self {
            Self(events.into_iter().map(|event| Ok(Some(event))).collect())
        }

        fn failing(message: &str) -> Self {
            Self(VecDeque::from([Err(anyhow::anyhow!("{message}"))]))
        }
    }

    impl EventSource for FakeEvents {
        fn next_event(&mut self) -> impl Future<Output = Result<Option<NiriEvent>>> + Send {
            future::ready(self.0.pop_front().unwrap_or(Ok(None)))
        }
    }

    #[tokio::test]
    async fn test_watcher_ignores_an_activation_that_did_not_take_focus() {
        let niri = fake_niri();
        let (business, config_path) = business_with_config(&niri, "");
        business.add_sticky_window(1).await.unwrap();

        // A workspace becomes active on its output as soon as a window lands on
        // it; following that would drag a sticky window across monitors.
        let events = FakeEvents::new([NiriEvent::WorkspaceActivated {
            id: 9,
            focused: false,
        }]);
        run_watcher(business, events).await.unwrap();

        assert!(niri.moves().is_empty(), "moved: {:?}", niri.moves());
        cleanup(&config_path);
    }

    #[tokio::test]
    async fn test_watcher_follows_a_focused_workspace_activation() {
        let niri = fake_niri();
        let (business, config_path) = business_with_config(&niri, "");
        business.add_sticky_window(1).await.unwrap();

        let events = FakeEvents::new([NiriEvent::WorkspaceActivated {
            id: 9,
            focused: true,
        }]);
        run_watcher(business, events).await.unwrap();

        assert_eq!(niri.moved_to_id(9), vec![1]);
        cleanup(&config_path);
    }

    #[tokio::test]
    async fn test_watcher_keeps_the_parking_areas_at_the_end() {
        let niri = Arc::new(FakeNiri::with_windows(&[
            (1, "fake", "Fake One"),
            (2, "fake", "Fake Two"),
            (3, "fake", "Fake Three"),
        ]));
        let (business, config_path) = business_with_config(
            &niri,
            "stage-workspace = \"parking\"\nscratchpad-workspace = \"drop\"\n",
        );
        // Both areas are parked, then a window on the tail pushes them up.
        niri.set_workspaces(&[
            (1, "one", "DP-1"),
            (2, "drop", "DP-1"),
            (3, "parking", "DP-1"),
            (5, "five", "DP-1"),
            (6, "", "DP-1"),
        ]);
        niri.set_window_workspace(1, 1);
        niri.set_window_workspace(2, 2);
        niri.set_window_workspace(3, 3);

        let events = FakeEvents::new([NiriEvent::WorkspacesChanged]);
        run_watcher(business, events).await.unwrap();

        assert_eq!(
            niri.workspace_naming().join("; "),
            "movews 2 5; movews 3 6",
            "both areas go back below every workspace the user works on"
        );
        cleanup(&config_path);
    }

    #[tokio::test]
    async fn test_watcher_forwards_window_events() {
        let niri = fake_niri();
        let (business, config_path) =
            business_with_config(&niri, "[sticky.browser]\napp-id = \"firefox\"\n");

        run_watcher(
            business.clone(),
            FakeEvents::new([NiriEvent::WindowOpenedOrChanged {
                id: 1,
                app_id: Some("firefox".into()),
                title: Some("Inbox".into()),
                floating: false,
            }]),
        )
        .await
        .unwrap();
        assert_eq!(sticky_ids(&business).await, "[1]");

        run_watcher(
            business.clone(),
            FakeEvents::new([NiriEvent::WindowClosed { id: 1 }]),
        )
        .await
        .unwrap();
        assert_eq!(sticky_ids(&business).await, "[]");
        cleanup(&config_path);
    }

    #[tokio::test]
    async fn test_watcher_reloads_the_configuration_when_niri_does() {
        let niri = fake_niri();
        let (business, config_path) = business_with_config(&niri, "");

        // The rule appears on disk while the daemon runs: niri reloading its
        // own configuration is the cue to pick it up.
        std::fs::write(&config_path, "[sticky.browser]\napp-id = \"firefox\"\n").unwrap();
        let events = FakeEvents::new([
            NiriEvent::ConfigLoaded { failed: true },
            NiriEvent::ConfigLoaded { failed: false },
        ]);
        run_watcher(business.clone(), events).await.unwrap();

        business
            .handle_window_opened_or_changed(1, Some("firefox".into()), None, false)
            .await
            .unwrap();
        assert_eq!(sticky_ids(&business).await, "[1]");
        cleanup(&config_path);
    }

    #[tokio::test]
    async fn test_watcher_reports_a_failed_event_stream() {
        let (business, config_path) = business_with_config(&fake_niri(), "");

        let error = run_watcher(business, FakeEvents::failing("niri hung up"))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("niri hung up"), "{error}");
        cleanup(&config_path);
    }

    #[tokio::test]
    async fn test_watcher_survives_a_failing_compositor() {
        let niri = fake_niri();
        let (business, config_path) = business_with_config(&niri, "");
        niri.fail_queries(true);

        // Reordering the parking areas cannot succeed with the compositor gone,
        // and that must not take the watcher down with it.
        run_watcher(
            business.clone(),
            FakeEvents::new([NiriEvent::WorkspacesChanged]),
        )
        .await
        .unwrap();
        assert!(niri.moves().is_empty());

        // The compositor is back: the next event is handled as usual.
        niri.fail_queries(false);
        business.add_sticky_window(1).await.unwrap();
        run_watcher(
            business,
            FakeEvents::new([NiriEvent::WorkspaceActivated {
                id: 9,
                focused: true,
            }]),
        )
        .await
        .unwrap();

        assert_eq!(niri.moved_to_id(9), vec![1]);
        cleanup(&config_path);
    }

    #[tokio::test]
    async fn test_watcher_keeps_going_after_a_failed_auto_stage() {
        let niri = Arc::new(FakeNiri::with_windows(&[
            (1, "game", "A Game"),
            (2, "fake", "Fake"),
        ]));
        let (business, config_path) =
            business_with_config(&niri, "[stage.games]\napp-id = \"game\"\n");
        business.add_sticky_window(2).await.unwrap();
        niri.fail_move(1); // niri refuses to park the game

        let events = FakeEvents::new([
            NiriEvent::WindowOpenedOrChanged {
                id: 1,
                app_id: Some("game".into()),
                title: Some("A Game".into()),
                floating: false,
            },
            NiriEvent::WorkspaceActivated {
                id: 9,
                focused: true,
            },
        ]);
        run_watcher(business, events).await.unwrap();

        assert_eq!(
            niri.moved_to_id(9),
            vec![2],
            "the window that was not affected still followed the workspace"
        );
        cleanup(&config_path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_cli_server_keeps_serving_after_a_client_vanishes() {
        let path = socket_path();
        let listener = bind_cli_socket(&path, Takeover::Refuse).await.unwrap();
        tokio::spawn(accept_cli_connections(listener, business()));

        // A client that asks and leaves without reading its answer: answering
        // into a closed socket must not take the server down.
        {
            let stream = UnixStream::connect(&path).await.unwrap();
            let (reader, mut writer) = stream.into_split();
            writer.write_all(b"{\"command\":\"list\"}\n").await.unwrap();
            writer.flush().await.unwrap();
            drop(writer);
            drop(reader);
        }

        let reply = one_request(&path, r#"{"command":"list"}"#).await;
        assert!(reply.contains(r#""status":"data""#), "{reply}");

        cleanup(&path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_daemon_serves_the_cli_without_a_compositor_connection() {
        let path = socket_path();
        let listener = bind_cli_socket(&path, Takeover::Refuse).await.unwrap();
        let server = tokio::spawn(serve(
            listener,
            Err(anyhow::anyhow!("niri is unreachable")),
            business(),
        ));

        // The watcher is dead, but the CLI keeps working: that is what a
        // restart, or looking at the state while niri is gone, needs.
        let reply = one_request(&path, r#"{"command":"list"}"#).await;
        assert!(reply.contains(r#""status":"data""#), "{reply}");

        server.abort();
        cleanup(&path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_replace_takes_the_socket_from_a_running_daemon() {
        let path = socket_path();
        let first = bind_cli_socket(&path, Takeover::Refuse).await.unwrap();
        let bound_by_first = socket_inode(&path);
        let first_daemon = tokio::spawn(serve(
            first,
            Err(anyhow::anyhow!("niri is unreachable")),
            business(),
        ));
        assert!(
            one_request(&path, r#"{"command":"list"}"#)
                .await
                .contains(r#""status":"data""#)
        );

        // A second daemon started by accident leaves the socket alone.
        let error = bind_cli_socket(&path, Takeover::Refuse).await.unwrap_err();
        assert!(error.to_string().contains("already running"), "{error}");

        // `nsticky --replace` takes it over on purpose: the path is bound
        // again, and whoever holds it answers.
        let second = bind_cli_socket(&path, Takeover::Replace).await.unwrap();
        assert_ne!(
            socket_inode(&path),
            bound_by_first,
            "the path was bound again"
        );
        let second_daemon = tokio::spawn(serve(
            second,
            Err(anyhow::anyhow!("niri is unreachable")),
            business(),
        ));

        let reply = one_request(&path, r#"{"command":"list"}"#).await;
        assert!(reply.contains(r#""status":"data""#), "{reply}");

        second_daemon.abort();
        first_daemon.abort();
        cleanup(&path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_cli_server_answers_requests() {
        let path = socket_path();
        let listener = bind_cli_socket(&path, Takeover::Refuse).await.unwrap();
        tokio::spawn(accept_cli_connections(listener, business()));

        let reply = one_request(&path, r#"{"command":"add","window_id":1}"#).await;
        assert!(reply.contains(r#""status":"success""#), "{reply}");

        let reply = one_request(&path, r#"{"command":"list"}"#).await;
        assert!(reply.contains(r#""data":"[1]""#), "{reply}");

        cleanup(&path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_cli_server_reports_invalid_json() {
        let path = socket_path();
        let listener = bind_cli_socket(&path, Takeover::Refuse).await.unwrap();
        tokio::spawn(accept_cli_connections(listener, business()));

        let reply = one_request(&path, "{ not json }").await;
        assert!(reply.contains("Failed to parse request"), "{reply}");

        cleanup(&path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_cli_server_bounds_request_size() {
        let path = socket_path();
        let listener = bind_cli_socket(&path, Takeover::Refuse).await.unwrap();
        tokio::spawn(accept_cli_connections(listener, business()));

        // Far beyond MAX_REQUEST_BYTES and without a newline: the daemon answers
        // an error instead of buffering it all.
        let huge = format!(r#"{{"command":"add","window_id":{}}}"#, "1".repeat(200_000));
        let reply = one_request(&path, &huge).await;
        assert!(reply.contains(r#""status":"error""#), "{reply}");

        cleanup(&path);
    }

    #[tokio::test]
    async fn test_bind_creates_the_socket_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = work_dir();
        let path = dir.join("run").join("nsticky").join("cli.sock");
        let _listener = bind_cli_socket(&path, Takeover::Refuse).await.unwrap();

        assert!(path.exists(), "{path:?} was not bound");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_bind_refuses_to_hijack_a_running_daemon() {
        let path = socket_path();
        let _live = bind_cli_socket(&path, Takeover::Refuse).await.unwrap();

        let error = bind_cli_socket(&path, Takeover::Refuse).await.unwrap_err();
        assert!(error.to_string().contains("already running"), "{error}");

        // ... unless the user asked for it.
        assert!(bind_cli_socket(&path, Takeover::Replace).await.is_ok());

        cleanup(&path);
    }

    #[tokio::test]
    async fn test_bind_replaces_a_stale_socket() {
        let path = socket_path();
        // A socket left behind by a crashed daemon: no one is listening.
        let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(stale);

        let bound = bind_cli_socket(&path, Takeover::Refuse).await;
        assert!(
            bound.is_ok(),
            "a stale socket file must not look like a running daemon: {:?} (connectable again: {})",
            bound.err(),
            UnixStream::connect(&path).await.is_ok()
        );

        cleanup(&path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_socket_is_only_accessible_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;

        let path = socket_path();
        let _listener = bind_cli_socket(&path, Takeover::Refuse).await.unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");

        cleanup(&path);
    }

    #[tokio::test]
    async fn test_read_bounded_line_keeps_short_lines_intact() {
        let mut reader = BufReader::new(&b"one\ntwo\n"[..]);

        let (line, truncated) = read_bounded_line(&mut reader, 64).await.unwrap();
        assert_eq!((line.as_str(), truncated), ("one", false));

        let (line, truncated) = read_bounded_line(&mut reader, 64).await.unwrap();
        assert_eq!((line.as_str(), truncated), ("two", false));
    }

    #[tokio::test]
    async fn test_read_bounded_line_cuts_an_oversized_line_at_the_limit() {
        let mut reader = BufReader::new(&b"aaaaaaaaaa\nnext\n"[..]);

        let (line, truncated) = read_bounded_line(&mut reader, 4).await.unwrap();
        assert!(truncated);
        assert_eq!(line, "aaaa", "at most the limit is kept");

        // The rest of the oversized line is discarded, so the reader is in step
        // with the client again.
        let (line, truncated) = read_bounded_line(&mut reader, 8).await.unwrap();
        assert_eq!((line.as_str(), truncated), ("next", false));
    }

    #[tokio::test]
    async fn test_read_bounded_line_returns_a_last_line_without_a_newline() {
        let mut reader = BufReader::new(&b"tail"[..]);

        let (line, truncated) = read_bounded_line(&mut reader, 64).await.unwrap();
        assert_eq!((line.as_str(), truncated), ("tail", false));
    }

    #[tokio::test]
    async fn test_read_bounded_line_reads_nothing_from_an_empty_reader() {
        let mut reader = BufReader::new(&b""[..]);

        let (line, truncated) = read_bounded_line(&mut reader, 64).await.unwrap();
        assert_eq!((line.as_str(), truncated), ("", false));
    }

    #[tokio::test]
    async fn test_cli_connection_says_nothing_to_a_client_that_sends_nothing() {
        assert!(replies_to(b"").await.is_empty());
    }

    #[tokio::test]
    async fn test_cli_connection_reports_bytes_that_are_not_a_request() {
        let requests: [&[u8]; 3] = [
            b"not json\n",
            // Invalid UTF-8: decoding must not fail the connection.
            b"\xff\xfe\x00\n",
            b"{\"command\":\"nope\"}\n",
        ];

        for request in requests {
            let replies = replies_to(request).await;
            assert_eq!(replies.len(), 1, "{request:?} -> {replies:?}");
            assert!(
                replies[0].contains("Failed to parse request"),
                "{request:?} -> {replies:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_cli_connection_answers_several_requests_in_order() {
        let replies = replies_to(
            b"{\"command\":\"add\",\"window_id\":1}\n{\"command\":\"list\"}\n{\"command\":\"bogus\"}\n",
        )
        .await;

        assert_eq!(replies.len(), 3, "{replies:?}");
        assert!(replies[0].contains(r#""status":"success""#), "{replies:?}");
        assert!(replies[1].contains(r#""data":"[1]""#), "{replies:?}");
        assert!(
            replies[2].contains("Failed to parse request"),
            "{replies:?}"
        );
    }

    #[tokio::test]
    async fn test_cli_connection_keeps_going_after_an_oversized_request() {
        let mut request = b"{\"command\":\"add\",\"window_id\":".to_vec();
        request.resize(request.len() + 200_000, b'1');
        request.extend_from_slice(b"}\n{\"command\":\"list\"}\n");

        let replies = replies_to(&request).await;

        assert_eq!(replies.len(), 2, "{replies:?}");
        assert!(replies[0].contains("Request too large"), "{replies:?}");
        assert!(replies[1].contains(r#""data":"[]""#), "{replies:?}");
    }

    #[tokio::test]
    async fn test_cli_connection_rejects_a_request_cut_inside_a_character() {
        // 30000 three-byte characters: the limit lands mid-character, which
        // must not panic or lose the error reply.
        let mut request = "\u{20ac}".repeat(30_000).into_bytes();
        request.push(b'\n');

        let replies = replies_to(&request).await;

        assert_eq!(replies.len(), 1, "{replies:?}");
        assert!(replies[0].contains("Request too large"), "{replies:?}");
    }

    #[tokio::test]
    async fn test_reload_reports_what_it_read() {
        let (business, config_path) = business_with_config(&fake_niri(), "");
        std::fs::write(
            &config_path,
            "[sticky.a]\napp-id = \"a\"\n\n[stage.b]\napp-id = \"b\"\n",
        )
        .unwrap();

        let message = reload(&business, "SIGHUP").await.unwrap();

        assert!(message.contains("2 rule(s)"), "{message}");
        cleanup(&config_path);
    }

    #[tokio::test]
    async fn test_reload_keeps_the_running_configuration_when_the_file_is_broken() {
        let (business, config_path) =
            business_with_config(&fake_niri(), "[sticky.good]\napp-id = \"fake\"\n");
        std::fs::write(&config_path, "[sticky.broken]\napp-id = \"[\"\n").unwrap();

        let error = reload(&business, "SIGHUP").await.unwrap_err();

        // The daemon is still running with the configuration it had: a typo in
        // the file must not stop it from managing windows.
        assert!(
            format!("{error:#}").contains("Keeping the running configuration"),
            "{error:#}"
        );
        business
            .handle_window_opened_or_changed(1, Some("fake".into()), None, false)
            .await
            .unwrap();
        assert_eq!(sticky_ids(&business).await, "[1]");
        cleanup(&config_path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_sighup_reloads_the_configuration() {
        let niri = fake_niri();
        let (business, config_path) = business_with_config(&niri, "");
        business
            .handle_window_opened_or_changed(1, Some("firefox".into()), None, false)
            .await
            .unwrap();
        assert_eq!(sticky_ids(&business).await, "[]");

        // Register a handler first: an unheard SIGHUP would end the test.
        let _registered =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).unwrap();
        tokio::spawn(watch_hangup(business.clone()));

        std::fs::write(&config_path, "[sticky.browser]\napp-id = \"firefox\"\n").unwrap();
        let pid = std::process::id().to_string();
        let mut reloaded = false;
        for _ in 0..50 {
            let status = std::process::Command::new("kill")
                .args(["-s", "HUP", &pid])
                .status()
                .unwrap();
            assert!(status.success(), "could not signal the test process");

            // The rule from disk applies to the next event, so the window tells
            // whether the reload happened.
            business
                .handle_window_opened_or_changed(1, Some("firefox".into()), None, false)
                .await
                .unwrap();
            if sticky_ids(&business).await == "[1]" {
                reloaded = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert!(reloaded, "SIGHUP did not reload the configuration");
        cleanup(&config_path);
    }
}
