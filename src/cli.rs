use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

use crate::config::{Config, RuleSummary, WindowAction};
use crate::menu::{MenuSpec, SELECTOR_CANDIDATES, Selector, is_available, resolve_selector};
use crate::niri::WindowInfo;
use crate::protocol;

#[derive(Parser, Debug)]
#[command(name = "nsticky")]
#[command(version)]
#[command(about = "Manage sticky windows via CLI", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
    /// Print machine-readable JSON instead of a table (list commands).
    #[arg(long, global = true)]
    json: bool,
}

/// Manage sticky windows across all workspaces.
#[derive(Subcommand, Debug)]
enum Commands {
    Sticky {
        #[command(subcommand)]
        action: StickyAction,
    },
    /// Manage staged windows (temporarily hidden in stage workspace).
    Stage {
        #[command(subcommand)]
        action: StageAction,
    },
    /// Report what nsticky can see: daemon, niri, paths and counts.
    Status,
    /// Re-read config.toml and apply it without restarting the daemon.
    Reload,
    /// Toggle a scratchpad from `[scratchpad.<name>]`: hide it, show it, or
    /// start it if it is not running. Without a name, toggles the focused
    /// window.
    Scratchpad {
        /// Scratchpad to toggle; omitted means the focused window.
        name: Option<String>,
    },
    /// Inspect the configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// List open windows with optional filters.
    Windows {
        /// Filter by application ID (partial match).
        #[arg(long)]
        app_id: Option<String>,
        /// Filter by window title (partial match).
        #[arg(long)]
        title: Option<String>,
    },
}

/// Actions for sticky windows.
#[derive(Subcommand, Debug)]
enum StickyAction {
    /// Add a window to sticky list by window ID.
    #[command(alias = "a")]
    Add { window_id: u64 },
    /// Remove a window from sticky list by window ID.
    #[command(alias = "r")]
    Remove { window_id: u64 },
    /// List all sticky windows.
    #[command(alias = "l")]
    List,
    /// Toggle sticky state of the currently active window.
    #[command(alias = "t")]
    ToggleActive,
    /// Toggle sticky state of windows matching the given app ID.
    #[command(alias = "ta")]
    ToggleAppid { appid: String },
    /// Toggle sticky state of windows matching the given title.
    #[command(alias = "tt")]
    ToggleTitle { title: String },
}

/// Actions for the configuration.
#[derive(Subcommand, Debug)]
enum ConfigAction {
    /// Validate config.toml, list its rules and exit.
    Check,
}

/// Actions for staged windows.
#[derive(Subcommand, Debug)]
enum StageAction {
    /// List all staged windows.
    #[command(alias = "l")]
    List,
    /// Stage a window by ID (move to stage workspace).
    #[command(alias = "a")]
    Add { window_id: u64 },
    /// Unstage a window by ID (move back from stage workspace).
    #[command(alias = "r")]
    Remove { window_id: u64 },
    /// Toggle stage state of the currently active window.
    #[command(alias = "t")]
    ToggleActive,
    /// Toggle stage state of windows matching the given app ID.
    #[command(alias = "ta")]
    ToggleAppid { appid: String },
    /// Toggle stage state of windows matching the given title.
    #[command(alias = "tt")]
    ToggleTitle { title: String },
    /// Stage all sticky windows.
    #[command(alias = "aa")]
    AddAll,
    /// Unstage all staged windows.
    #[command(alias = "ra")]
    RemoveAll,
    /// Interactive restore: list staged windows with names and pick one to unstage.
    #[command(alias = "rs")]
    Restore,
}

impl Cli {
    pub fn into_request(self) -> protocol::Request {
        match self.command {
            Commands::Sticky { action } => match action {
                StickyAction::Add { window_id } => protocol::Request::Add { window_id },
                StickyAction::Remove { window_id } => protocol::Request::Remove { window_id },
                StickyAction::List => protocol::Request::List,
                StickyAction::ToggleActive => protocol::Request::ToggleActive,
                StickyAction::ToggleAppid { appid } => protocol::Request::ToggleAppid { appid },
                StickyAction::ToggleTitle { title } => protocol::Request::ToggleTitle { title },
            },
            Commands::Stage { action } => match action {
                StageAction::List => protocol::Request::StageList,
                StageAction::Add { window_id } => protocol::Request::Stage { window_id },
                StageAction::Remove { window_id } => protocol::Request::Unstage { window_id },
                StageAction::ToggleActive => protocol::Request::StageToggleActive,
                StageAction::ToggleAppid { appid } => protocol::Request::StageToggleAppid { appid },
                StageAction::ToggleTitle { title } => protocol::Request::StageToggleTitle { title },
                StageAction::AddAll => protocol::Request::StageAll,
                StageAction::RemoveAll => protocol::Request::UnstageAll,
                StageAction::Restore => unreachable!("Restore is handled separately"),
            },
            Commands::Status => unreachable!("Status is handled separately"),
            Commands::Reload => protocol::Request::Reload,
            Commands::Scratchpad { name } => protocol::Request::Scratchpad { name },
            Commands::Config { .. } => unreachable!("Config is handled separately"),
            Commands::Windows { .. } => protocol::Request::Windows,
        }
    }
}

/// Sockets and files the CLI talks to. `run_cli` reads them from the
/// environment; commands take them as an argument so tests can redirect them.
#[derive(Debug, Clone)]
struct Endpoints {
    socket: PathBuf,
    config: PathBuf,
    state: PathBuf,
}

impl Endpoints {
    fn from_env() -> Self {
        Self {
            socket: protocol::cli_socket_path(),
            config: Config::default_config_path(),
            state: crate::state_store::default_path(),
        }
    }

    /// One request/response round trip over a connection of its own.
    async fn exchange(&self, request: &protocol::Request) -> Result<protocol::Response> {
        let stream = UnixStream::connect(&self.socket).await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let json = serde_json::to_string(request)?;
        writer.write_all(json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        serde_json::from_str(line.trim()).context("Invalid JSON response from daemon")
    }

    /// Ask the daemon for its window list and keep the entries whose id is in
    /// `ids`, in the order it reports them.
    async fn fetch_windows_matching(&self, ids: &[u64]) -> Result<Vec<WindowInfo>> {
        match self.exchange(&protocol::Request::Windows).await? {
            protocol::Response::Data { data } => {
                let windows: Vec<WindowInfo> = serde_json::from_str(&data)?;
                Ok(windows
                    .into_iter()
                    .filter(|window| ids.contains(&window.id))
                    .collect())
            }
            protocol::Response::Error { message } => bail!("{message}"),
            other => bail!("Unexpected daemon response: {other:?}"),
        }
    }

    /// Window ids sitting in the stage workspace right now.
    async fn stage_ids(&self) -> Result<Vec<u64>> {
        match self.exchange(&protocol::Request::StageList).await? {
            protocol::Response::Data { data } => Ok(serde_json::from_str(&data)?),
            protocol::Response::Error { message } => bail!("{message}"),
            other => bail!("Unexpected daemon response: {other:?}"),
        }
    }

    /// Unstage one window, returning the confirmation to show the user.
    async fn unstage(&self, window_id: u64) -> Result<String> {
        match self
            .exchange(&protocol::Request::Unstage { window_id })
            .await?
        {
            protocol::Response::Success { message } => Ok(message),
            protocol::Response::Error { message } => bail!("{message}"),
            other => bail!("Unexpected daemon response: {other:?}"),
        }
    }
}

/// Fallback used when no external selector is configured: the user types the
/// numbers of the windows to restore (space separated), `q` cancels.
async fn run_terminal_restore(
    endpoints: &Endpoints,
    staged: &[WindowInfo],
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<()> {
    writeln!(out)?;
    for (i, window) in staged.iter().enumerate() {
        let app_id = window.app_id.as_deref().unwrap_or("?");
        let title = window.title.as_deref().unwrap_or("?");
        writeln!(
            out,
            " {}. {} — {}  [ID: {}]",
            i + 1,
            app_id,
            title,
            window.id
        )?;
    }
    writeln!(out)?;
    write!(
        out,
        "Restore (1-{}, space-separated for multiple, q): ",
        staged.len()
    )?;
    // The prompt has no newline, so it needs this flush before the read blocks.
    out.flush()?;

    let mut line = String::new();
    input.read_line(&mut line)?;
    let line = line.trim();
    if line.eq_ignore_ascii_case("q") || line.is_empty() {
        return Ok(());
    }

    for token in line.split_whitespace() {
        if let Ok(number) = token.parse::<usize>()
            && let Some(window) = number.checked_sub(1).and_then(|i| staged.get(i))
        {
            writeln!(out, "{}", endpoints.unstage(window.id).await?)?;
        }
    }
    Ok(())
}

/// Where `stage restore` looks for a selector. Read from the environment in
/// production; tests supply their own so a real selector is never launched.
struct RestoreEnv {
    menu: Option<MenuSpec>,
    from_env: Option<MenuSpec>,
    stdin_is_terminal: bool,
    /// How a selector is looked up on `PATH`.
    available: fn(&str) -> bool,
}

impl RestoreEnv {
    fn from_settings(config: &Config) -> Self {
        Self {
            menu: config.menu().cloned(),
            from_env: MenuSpec::from_env(),
            stdin_is_terminal: std::io::stdin().is_terminal(),
            available: is_available,
        }
    }
}

async fn run_stage_restore(
    endpoints: &Endpoints,
    env: RestoreEnv,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<()> {
    let ids = endpoints.stage_ids().await?;

    let spec = match resolve_selector(env.menu, env.from_env, env.stdin_is_terminal, env.available)
    {
        Selector::Command(spec) => spec,
        Selector::Terminal => {
            // Short-circuit so an empty list never prints a prompt.
            if ids.is_empty() {
                writeln!(out, "No staged windows.")?;
                return Ok(());
            }
            let staged = endpoints.fetch_windows_matching(&ids).await?;
            if staged.is_empty() {
                writeln!(out, "No active staged windows found.")?;
                return Ok(());
            }
            return run_terminal_restore(endpoints, &staged, input, out).await;
        }
        Selector::Unavailable => {
            let candidates: Vec<&str> = SELECTOR_CANDIDATES
                .iter()
                .map(|(program, _)| *program)
                .collect();
            bail!(
                "No selector available: stdin is not a terminal and no `menu` is configured.\n\
                 Set one in config.toml or NSTICKY_MENU, for example:\n\
                 \tmenu = \"vicinae dmenu --placeholder 'Restore Window:'\"\n\
                 Recommended selector: Vicinae (https://www.vicinae.com/)\n\
                 Known selectors: {}",
                candidates.join(", ")
            );
        }
    };

    // Open the selector even when nothing is staged: it shows its own empty
    // state, so a bound shortcut never looks dead.
    let staged = endpoints.fetch_windows_matching(&ids).await?;
    run_menu_restore(endpoints, &spec, &staged, out, err).await
}

/// Feed the selector one `"<id>\t<app_id> — <title>"` line per staged window and
/// unstage every id its stdout holds (a multi-select prints several lines).
async fn run_menu_restore(
    endpoints: &Endpoints,
    spec: &MenuSpec,
    staged: &[WindowInfo],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<()> {
    let menu = spec.parse()?;
    let mut child = menu.spawn()?;

    let input = staged
        .iter()
        .map(|w| {
            format!(
                "{}\t{} — {}",
                w.id,
                w.app_id.as_deref().unwrap_or("?"),
                w.title.as_deref().unwrap_or("?")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input.as_bytes())?;
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        // Never fail silently: for a bound shortcut, this status is the clue.
        writeln!(
            err,
            "Menu exited with {}\n\n{}",
            output.status,
            menu.describe()
        )?;
        return Ok(());
    }

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(window_id) = first_window_id(line) {
            writeln!(out, "{}", endpoints.unstage(window_id).await?)?;
        }
    }
    Ok(())
}

/// First integer of a selector output line: the id nsticky prints at the start
/// of every entry it feeds the selector.
fn first_window_id(line: &str) -> Option<u64> {
    static ID: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\d+").expect("valid id regex"));
    ID.find(line.trim())?.as_str().parse().ok()
}

/// Machine-readable form of `nsticky config check`.
#[derive(Debug, serde::Serialize)]
struct ConfigReport<'a> {
    config: String,
    state: String,
    stage_workspace: &'a str,
    stage_keep_workspace: bool,
    scratchpad_workspace: &'a str,
    sticky_follow: &'a str,
    scratchpads: Vec<ScratchpadReport>,
    menu: Option<String>,
    rules: Vec<RuleReport>,
}

#[derive(Debug, serde::Serialize)]
struct ScratchpadReport {
    name: String,
    size: String,
    float: bool,
    spawn: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
struct RuleReport {
    action: &'static str,
    id: String,
    outputs: Vec<String>,
    app_id: usize,
    title: usize,
    exclude_app_id: usize,
    exclude_title: usize,
}

/// Validate `config.toml` and report what nsticky made of it.
fn run_config_check(endpoints: &Endpoints, json: bool, out: &mut dyn Write) -> Result<()> {
    let path = &endpoints.config;
    let config = Config::load(path)?;

    if json {
        let report = ConfigReport {
            config: path.display().to_string(),
            state: endpoints.state.display().to_string(),
            stage_workspace: config.stage_workspace(),
            stage_keep_workspace: config.stage_keep_workspace(),
            scratchpad_workspace: config.scratchpad_workspace(),
            sticky_follow: config.sticky_follow().as_str(),
            scratchpads: describe_scratchpads(&config),
            menu: config.menu().map(|menu| menu.describe()),
            rules: config
                .rules()
                .into_iter()
                .map(|(action, rule)| RuleReport {
                    action: action_label(action),
                    id: rule.id,
                    outputs: rule.outputs,
                    app_id: rule.app_id,
                    title: rule.title,
                    exclude_app_id: rule.exclude_app_id,
                    exclude_title: rule.exclude_title,
                })
                .collect(),
        };
        writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
        return Ok(());
    }

    writeln!(out, "config:          {}", path.display())?;
    writeln!(out, "state:           {}", endpoints.state.display())?;
    writeln!(
        out,
        "stage workspace: {} (kept while empty: {})",
        config.stage_workspace(),
        config.stage_keep_workspace()
    )?;
    writeln!(
        out,
        "scratchpad:      {} (windows parked there)",
        config.scratchpad_workspace()
    )?;
    writeln!(out, "sticky follow:   {}", config.sticky_follow().as_str())?;
    writeln!(
        out,
        "menu:            {}",
        match config.menu() {
            Some(menu) => menu.describe(),
            None => "(none, `stage restore` falls back to the terminal)".to_string(),
        }
    )?;

    let scratchpads = describe_scratchpads(&config);
    if scratchpads.is_empty() {
        writeln!(out, "scratchpads:     (none)")?;
    } else {
        writeln!(out, "scratchpads:")?;
        for scratchpad in &scratchpads {
            let spawn = if scratchpad.spawn.is_empty() {
                "(no spawn command)".to_string()
            } else {
                format!("spawn: {}", scratchpad.spawn.join(" "))
            };
            writeln!(
                out,
                "  {:<8} {:<12} {:<8} {}",
                scratchpad.name,
                scratchpad.size,
                if scratchpad.float {
                    "floating"
                } else {
                    "tiled"
                },
                spawn
            )?;
        }
    }

    let rules = config.rules();
    if rules.is_empty() {
        writeln!(out, "rules:           (none)")?;
    } else {
        writeln!(out, "rules:")?;
        for (action, rule) in rules {
            writeln!(
                out,
                "  {:<6} {}",
                action_label(action),
                describe_rule(&rule)
            )?;
        }
    }

    Ok(())
}

/// Configured scratchpads, for `config check` (text and JSON).
fn describe_scratchpads(config: &Config) -> Vec<ScratchpadReport> {
    config
        .scratchpad_names()
        .into_iter()
        .filter_map(|name| config.scratchpad(name))
        .map(|scratchpad| ScratchpadReport {
            name: scratchpad.name.clone(),
            size: scratchpad.describe_size(),
            float: scratchpad.float,
            spawn: scratchpad.spawn.clone().unwrap_or_default(),
        })
        .collect()
}

fn action_label(action: WindowAction) -> &'static str {
    match action {
        WindowAction::Sticky => "sticky",
        WindowAction::Stage => "stage",
    }
}

/// One rule as a line: `discord  app-id: 3, exclude-title: 3`.
fn describe_rule(rule: &RuleSummary) -> String {
    let name = rule
        .id
        .split_once('.')
        .map(|(_, name)| name)
        .unwrap_or(&rule.id);
    format!("{name}  {}", rule.fields())
}

/// What `nsticky status` managed to find out.
struct Status {
    socket: std::path::PathBuf,
    config: std::path::PathBuf,
    state: std::path::PathBuf,
    /// Sticky/staged counts and whether niri answered; `None` means the daemon
    /// did not answer at all.
    daemon: Option<DaemonStatus>,
}

struct DaemonStatus {
    sticky: usize,
    staged: usize,
    niri_error: Option<String>,
}

impl Status {
    fn render(&self) -> String {
        let mut lines = vec![
            format!("socket: {}", self.socket.display()),
            format!("config: {}", self.config.display()),
            format!("state:  {}", self.state.display()),
        ];

        match &self.daemon {
            None => lines.push("daemon: not running".to_string()),
            Some(daemon) => {
                lines.push("daemon: running".to_string());
                lines.push(format!("sticky: {} window(s)", daemon.sticky));
                lines.push(format!("staged: {} window(s)", daemon.staged));
                lines.push(match &daemon.niri_error {
                    None => "niri:   connected".to_string(),
                    Some(error) => format!("niri:   unreachable ({error})"),
                });
            }
        }

        lines.join("\n")
    }
}

/// Report the state of the daemon, the compositor and the configuration.
async fn run_status(endpoints: &Endpoints, out: &mut dyn Write) -> Result<()> {
    let mut status = Status {
        socket: endpoints.socket.clone(),
        config: endpoints.config.clone(),
        state: endpoints.state.clone(),
        daemon: None,
    };

    let sticky = endpoints.exchange(&protocol::Request::List).await;
    if let Ok(protocol::Response::Data { data }) = &sticky {
        let status_ids = |data: &str| -> usize {
            serde_json::from_str::<Vec<u64>>(data).map_or(0, |ids| ids.len())
        };
        let staged = match endpoints.exchange(&protocol::Request::StageList).await {
            Ok(protocol::Response::Data { data }) => status_ids(&data),
            _ => 0,
        };
        let niri_error = match endpoints.exchange(&protocol::Request::Windows).await {
            Ok(protocol::Response::Data { .. }) => None,
            Ok(protocol::Response::Error { message }) => Some(message),
            Ok(other) => Some(format!("unexpected response: {other:?}")),
            Err(e) => Some(format!("{e:#}")),
        };

        status.daemon = Some(DaemonStatus {
            sticky: status_ids(data),
            staged,
            niri_error,
        });
    }

    writeln!(out, "{}", status.render())?;

    // Print the report even without a daemon (it names the socket that was
    // tried), but keep the exit status an error.
    if status.daemon.is_none() {
        bail!("The nsticky daemon is not running");
    }
    Ok(())
}

pub async fn run_cli() -> Result<()> {
    let cli = Cli::parse();
    let endpoints = Endpoints::from_env();
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    dispatch(cli, &endpoints, &mut out, &mut err).await
}

/// Run one parsed command: `out` takes the rendered result, `err` the notices
/// that must not land in a script's captured stdout.
async fn dispatch(
    cli: Cli,
    endpoints: &Endpoints,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<()> {
    match &cli.command {
        Commands::Stage {
            action: StageAction::Restore,
        } => {
            let config = Config::load_or_default();
            let mut input = std::io::stdin().lock();
            return run_stage_restore(
                endpoints,
                RestoreEnv::from_settings(&config),
                &mut input,
                out,
                err,
            )
            .await;
        }
        Commands::Config {
            action: ConfigAction::Check,
        } => return run_config_check(endpoints, cli.json, out),
        Commands::Status => return run_status(endpoints, out).await,
        _ => {}
    }

    let filter_args = match &cli.command {
        Commands::Windows { app_id, title } => (app_id.clone(), title.clone()),
        _ => (None, None),
    };

    let view = data_view(&cli.command);
    let json = cli.json;
    let request = cli.into_request();

    match endpoints.exchange(&request).await? {
        protocol::Response::Success { message } => {
            writeln!(out, "{message}")?;
            Ok(())
        }
        // A failed command must not exit 0: scripts and keybindings rely on it.
        protocol::Response::Error { message } => bail!("{message}"),
        protocol::Response::Data { data } => {
            render_data(endpoints, view, &data, json, &filter_args, out).await
        }
    }
}

/// How a `Data` response should be presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataView {
    /// JSON list of windows, shown as a table unless `--json` was passed.
    Windows,
    /// JSON list of window ids, joined with window details for the table.
    Ids,
    /// Anything else: printed verbatim.
    Raw,
}

fn data_view(command: &Commands) -> DataView {
    match command {
        Commands::Windows { .. } => DataView::Windows,
        Commands::Sticky {
            action: StickyAction::List,
        }
        | Commands::Stage {
            action: StageAction::List,
        } => DataView::Ids,
        _ => DataView::Raw,
    }
}

async fn render_data(
    endpoints: &Endpoints,
    view: DataView,
    data: &str,
    json: bool,
    filters: &(Option<String>, Option<String>),
    out: &mut dyn Write,
) -> Result<()> {
    match view {
        DataView::Windows => {
            let windows: Vec<WindowInfo> =
                serde_json::from_str(data).context("Failed to parse window list")?;
            let windows = filter_windows(windows, filters);
            if json {
                writeln!(out, "{}", serde_json::to_string(&windows)?)?;
            } else {
                print_windows_table(&windows, out)?;
            }
            Ok(())
        }
        DataView::Ids => {
            let ids: Vec<u64> = serde_json::from_str(data).context("Failed to parse id list")?;
            if json {
                writeln!(out, "{}", serde_json::to_string(&ids)?)?;
                return Ok(());
            }
            // The daemon reports ids alone and the table needs app ids and
            // titles, hence the extra round trip.
            let mut windows = endpoints.fetch_windows_matching(&ids).await?;
            windows.sort_by_key(|window| window.id);
            print_windows_table(&windows, out)?;
            Ok(())
        }
        DataView::Raw => {
            writeln!(out, "{data}")?;
            Ok(())
        }
    }
}

/// Case-insensitive substring filters on app id and title, sorted by id so the
/// output is stable.
fn filter_windows(
    windows: Vec<WindowInfo>,
    filters: &(Option<String>, Option<String>),
) -> Vec<WindowInfo> {
    let (filter_app_id, filter_title) = filters;
    let matches = |filter: &Option<String>, value: &Option<String>| match (filter, value) {
        (Some(filter), Some(value)) => value.to_lowercase().contains(&filter.to_lowercase()),
        (Some(_), None) => false,
        (None, _) => true,
    };

    let mut filtered: Vec<WindowInfo> = windows
        .into_iter()
        .filter(|window| {
            matches(filter_app_id, &window.app_id) && matches(filter_title, &window.title)
        })
        .collect();
    filtered.sort_by_key(|window| window.id);
    filtered
}

fn print_windows_table(windows: &[WindowInfo], out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out, "{:<10} {:<25} TITLE", "ID", "APP_ID")?;
    for window in windows {
        let app_id = window.app_id.as_deref().unwrap_or("<unknown>");
        let title = window.title.as_deref().unwrap_or("<unknown>");
        writeln!(out, "{:<10} {:<25} {}", window.id, app_id, title)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use parking_lot::Mutex;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::UnixListener;

    /// A directory of its own for the socket, the config and the state file, so
    /// no test reads the user's config or opens the user's socket.
    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "nsticky-cli-test-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        /// Write `content` to `name` and return its path.
        fn file(&self, name: &str, content: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, content).unwrap();
            path
        }

        /// Write an executable `name`, for use as a selector command.
        fn script(&self, name: &str, body: &str) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let path = self.file(name, body);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        fn endpoints(&self) -> Endpoints {
            Endpoints {
                socket: self.0.join("cli.sock"),
                config: self.0.join("config.toml"),
                state: self.0.join("state.json"),
            }
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A stand-in for the daemon: answers the scripted replies in order and
    /// records every request it saw. A request past the end of the script gets
    /// an error, so an unexpected round trip fails instead of blocking.
    struct FakeDaemon {
        seen: Arc<Mutex<Vec<protocol::Request>>>,
    }

    impl FakeDaemon {
        async fn start(dir: &Dir, replies: Vec<protocol::Response>) -> Self {
            Self::raw(
                dir,
                replies
                    .iter()
                    .map(|reply| serde_json::to_string(reply).unwrap())
                    .collect(),
            )
            .await
        }

        /// Answer with raw lines, for replies a healthy daemon never sends.
        async fn raw(dir: &Dir, lines: Vec<String>) -> Self {
            let listener = UnixListener::bind(dir.endpoints().socket).unwrap();
            let seen: Arc<Mutex<Vec<protocol::Request>>> = Arc::default();
            let queued: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(lines.into()));

            tokio::spawn({
                let seen = Arc::clone(&seen);
                let queued = Arc::clone(&queued);
                async move {
                    loop {
                        let Ok((stream, _)) = listener.accept().await else {
                            return;
                        };
                        let seen = Arc::clone(&seen);
                        let queued = Arc::clone(&queued);
                        tokio::spawn(async move {
                            let (reader, mut writer) = stream.into_split();
                            let mut reader = BufReader::new(reader);
                            let mut line = String::new();
                            if reader.read_line(&mut line).await.is_err() {
                                return;
                            }
                            let Ok(request) =
                                serde_json::from_str::<protocol::Request>(line.trim())
                            else {
                                return;
                            };
                            seen.lock().push(request.clone());
                            let reply = queued.lock().pop_front().unwrap_or_else(|| {
                                serde_json::to_string(&protocol::Response::error(format!(
                                    "unscripted request: {request:?}"
                                )))
                                .unwrap()
                            });
                            let body = format!("{reply}\n");
                            let _ = writer.write_all(body.as_bytes()).await;
                        });
                    }
                }
            });

            Self { seen }
        }

        fn seen(&self) -> Vec<protocol::Request> {
            self.seen.lock().clone()
        }
    }

    /// Run `args` through argument parsing and the real dispatch, returning the
    /// result plus what the user would have seen on stdout and stderr.
    async fn run(endpoints: &Endpoints, args: &[&str]) -> (Result<()>, String, String) {
        let cli = Cli::try_parse_from(args).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = dispatch(cli, endpoints, &mut out, &mut err).await;
        (
            result,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    /// `stage restore` on a terminal, with `typed` on stdin. The env is spelled
    /// out because discovering a selector would launch the user's own.
    async fn terminal_restore(dir: &Dir, typed: &str) -> (Result<()>, String, String) {
        restore(
            dir,
            RestoreEnv {
                menu: None,
                from_env: None,
                stdin_is_terminal: true,
                available: |_| false,
            },
            typed,
        )
        .await
    }

    /// `stage restore` with a configured selector and no terminal.
    async fn menu_restore(dir: &Dir, spec: MenuSpec) -> (Result<()>, String, String) {
        restore(
            dir,
            RestoreEnv {
                menu: Some(spec),
                from_env: None,
                stdin_is_terminal: false,
                available: |_| false,
            },
            "",
        )
        .await
    }

    async fn restore(dir: &Dir, env: RestoreEnv, typed: &str) -> (Result<()>, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run_stage_restore(
            &dir.endpoints(),
            env,
            &mut std::io::Cursor::new(typed.to_string()),
            &mut out,
            &mut err,
        )
        .await;
        (
            result,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn windows_data(windows: &[WindowInfo]) -> protocol::Response {
        protocol::Response::data(serde_json::to_string(windows).unwrap())
    }

    fn ids_data(ids: &[u64]) -> protocol::Response {
        protocol::Response::data(serde_json::to_string(ids).unwrap())
    }

    /// A config exercising both parking areas, a scratchpad and multi-pattern
    /// rules with exclusions.
    const FULL_CONFIG: &str = r#"
menu = "rofi -dmenu"
stage-workspace = "parking"
stage-keep-workspace = true
sticky-follow = "own-output"

[scratchpad.term]
spawn = ["foot", "-a", "nsticky-term"]
width = "60%"
height = 900

[sticky.discord]
app-id = ["discord", "Vesktop"]
title = "Discord"
exclude-title = ["(muted)", "(deafened)"]

[stage.games]
app-id = ["steam"]
exclude-app-id = "steamwebhelper"
"#;

    #[test]
    fn test_cli_sticky_add() {
        let cli = Cli::try_parse_from(["nsticky", "sticky", "add", "42"]).unwrap();
        assert_eq!(cli.into_request(), protocol::Request::Add { window_id: 42 });
    }

    #[test]
    fn test_cli_sticky_remove() {
        let cli = Cli::try_parse_from(["nsticky", "sticky", "remove", "7"]).unwrap();
        assert_eq!(
            cli.into_request(),
            protocol::Request::Remove { window_id: 7 }
        );
    }

    #[test]
    fn test_cli_sticky_list() {
        let cli = Cli::try_parse_from(["nsticky", "sticky", "list"]).unwrap();
        assert_eq!(cli.into_request(), protocol::Request::List);
    }

    #[test]
    fn test_cli_sticky_toggle_active() {
        let cli = Cli::try_parse_from(["nsticky", "sticky", "toggle-active"]).unwrap();
        assert_eq!(cli.into_request(), protocol::Request::ToggleActive);
    }

    #[test]
    fn test_cli_sticky_toggle_appid() {
        let cli = Cli::try_parse_from(["nsticky", "sticky", "toggle-appid", "firefox"]).unwrap();
        assert_eq!(
            cli.into_request(),
            protocol::Request::ToggleAppid {
                appid: "firefox".to_string()
            }
        );
    }

    #[test]
    fn test_cli_sticky_toggle_title() {
        let cli = Cli::try_parse_from(["nsticky", "sticky", "toggle-title", "Gmail"]).unwrap();
        assert_eq!(
            cli.into_request(),
            protocol::Request::ToggleTitle {
                title: "Gmail".to_string()
            }
        );
    }

    #[test]
    fn test_cli_stage_list() {
        let cli = Cli::try_parse_from(["nsticky", "stage", "list"]).unwrap();
        assert_eq!(cli.into_request(), protocol::Request::StageList);
    }

    #[test]
    fn test_cli_stage_add() {
        let cli = Cli::try_parse_from(["nsticky", "stage", "add", "99"]).unwrap();
        assert_eq!(
            cli.into_request(),
            protocol::Request::Stage { window_id: 99 }
        );
    }

    #[test]
    fn test_cli_stage_remove() {
        let cli = Cli::try_parse_from(["nsticky", "stage", "remove", "10"]).unwrap();
        assert_eq!(
            cli.into_request(),
            protocol::Request::Unstage { window_id: 10 }
        );
    }

    #[test]
    fn test_cli_stage_toggle_active() {
        let cli = Cli::try_parse_from(["nsticky", "stage", "toggle-active"]).unwrap();
        assert_eq!(cli.into_request(), protocol::Request::StageToggleActive);
    }

    #[test]
    fn test_cli_stage_toggle_appid() {
        let cli = Cli::try_parse_from(["nsticky", "stage", "toggle-appid", "chromium"]).unwrap();
        assert_eq!(
            cli.into_request(),
            protocol::Request::StageToggleAppid {
                appid: "chromium".to_string()
            }
        );
    }

    #[test]
    fn test_cli_stage_toggle_title() {
        let cli = Cli::try_parse_from(["nsticky", "stage", "toggle-title", "Terminal"]).unwrap();
        assert_eq!(
            cli.into_request(),
            protocol::Request::StageToggleTitle {
                title: "Terminal".to_string()
            }
        );
    }

    #[test]
    fn test_cli_stage_add_all() {
        let cli = Cli::try_parse_from(["nsticky", "stage", "add-all"]).unwrap();
        assert_eq!(cli.into_request(), protocol::Request::StageAll);
    }

    #[test]
    fn test_cli_stage_remove_all() {
        let cli = Cli::try_parse_from(["nsticky", "stage", "remove-all"]).unwrap();
        assert_eq!(cli.into_request(), protocol::Request::UnstageAll);
    }

    #[test]
    fn test_cli_windows() {
        let cli = Cli::try_parse_from(["nsticky", "windows"]).unwrap();
        assert_eq!(cli.into_request(), protocol::Request::Windows);
        let cli = Cli::try_parse_from(["nsticky", "windows", "--app-id", "firefox"]).unwrap();
        assert_eq!(cli.into_request(), protocol::Request::Windows);
        let cli = Cli::try_parse_from(["nsticky", "windows", "--title", "gmail"]).unwrap();
        assert_eq!(cli.into_request(), protocol::Request::Windows);
    }

    #[test]
    fn test_cli_stage_restore() {
        let cli = Cli::try_parse_from(["nsticky", "stage", "restore"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Stage {
                action: StageAction::Restore
            }
        ));
    }

    #[test]
    fn test_cli_stage_restore_alias() {
        let cli = Cli::try_parse_from(["nsticky", "stage", "rs"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Stage {
                action: StageAction::Restore
            }
        ));
    }

    #[test]
    fn test_status_reports_a_missing_daemon() {
        let status = Status {
            socket: "/run/nsticky/cli.sock".into(),
            config: "/home/x/.config/nsticky/config.toml".into(),
            state: "/home/x/.local/state/nsticky/state.json".into(),
            daemon: None,
        };

        let rendered = status.render();
        assert!(rendered.contains("daemon: not running"), "{rendered}");
        assert!(!rendered.contains("sticky:"), "{rendered}");
    }

    #[test]
    fn test_status_reports_counts_and_compositor_state() {
        let with_niri = Status {
            socket: "s".into(),
            config: "c".into(),
            state: "st".into(),
            daemon: Some(DaemonStatus {
                sticky: 3,
                staged: 1,
                niri_error: None,
            }),
        };
        let rendered = with_niri.render();
        assert!(rendered.contains("daemon: running"), "{rendered}");
        assert!(rendered.contains("sticky: 3 window(s)"), "{rendered}");
        assert!(rendered.contains("staged: 1 window(s)"), "{rendered}");
        assert!(rendered.contains("niri:   connected"), "{rendered}");

        let without_niri = Status {
            socket: "s".into(),
            config: "c".into(),
            state: "st".into(),
            daemon: Some(DaemonStatus {
                sticky: 0,
                staged: 0,
                niri_error: Some("NIRI_SOCKET env var not set".into()),
            }),
        };
        let rendered = without_niri.render();
        assert!(
            rendered.contains("niri:   unreachable (NIRI_SOCKET env var not set)"),
            "{rendered}"
        );
    }

    #[test]
    fn test_describe_rule_reports_the_name_and_pattern_counts() {
        let rule = RuleSummary {
            floating: None,
            outputs: Vec::new(),
            id: "sticky.discord".to_string(),
            app_id: 3,
            title: 0,
            exclude_app_id: 0,
            exclude_title: 2,
        };
        assert_eq!(describe_rule(&rule), "discord  app-id: 3, exclude-title: 2");
    }

    #[test]
    fn test_config_report_is_machine_readable() {
        let rule = RuleSummary {
            floating: None,
            outputs: Vec::new(),
            id: "stage.games".to_string(),
            app_id: 2,
            title: 0,
            exclude_app_id: 0,
            exclude_title: 1,
        };
        let report = ConfigReport {
            config: "/home/x/.config/nsticky/config.toml".to_string(),
            state: "/home/x/.local/state/nsticky/state.json".to_string(),
            stage_workspace: "parking",
            stage_keep_workspace: false,
            scratchpad_workspace: "scratchpad",
            sticky_follow: "own-output",
            scratchpads: Vec::new(),
            menu: Some("rofi -dmenu".to_string()),
            rules: vec![RuleReport {
                action: action_label(WindowAction::Stage),
                id: rule.id.clone(),
                outputs: rule.outputs.clone(),
                app_id: rule.app_id,
                title: rule.title,
                exclude_app_id: rule.exclude_app_id,
                exclude_title: rule.exclude_title,
            }],
        };

        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(json["stage_workspace"], "parking");
        assert_eq!(json["scratchpad_workspace"], "scratchpad");
        assert_eq!(json["sticky_follow"], "own-output");
        assert_eq!(json["menu"], "rofi -dmenu");
        assert_eq!(json["rules"][0]["action"], "stage");
        assert_eq!(json["rules"][0]["id"], "stage.games");
        assert_eq!(json["rules"][0]["app_id"], 2);
        assert_eq!(json["rules"][0]["exclude_title"], 1);
    }

    #[test]
    fn test_cli_scratchpad() {
        let cli = Cli::try_parse_from(["nsticky", "scratchpad", "term"]).unwrap();
        assert_eq!(
            cli.into_request(),
            protocol::Request::Scratchpad {
                name: Some("term".to_string())
            }
        );

        let cli = Cli::try_parse_from(["nsticky", "scratchpad"]).unwrap();
        assert_eq!(
            cli.into_request(),
            protocol::Request::Scratchpad { name: None },
            "no name toggles the focused window"
        );
    }

    #[test]
    fn test_cli_reload() {
        let cli = Cli::try_parse_from(["nsticky", "reload"]).unwrap();
        assert_eq!(cli.into_request(), protocol::Request::Reload);
    }

    #[test]
    fn test_cli_status() {
        let cli = Cli::try_parse_from(["nsticky", "status"]).unwrap();
        assert!(matches!(cli.command, Commands::Status));
    }

    #[test]
    fn test_cli_config_check() {
        let cli = Cli::try_parse_from(["nsticky", "config", "check"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Config {
                action: ConfigAction::Check
            }
        ));
    }

    fn window(id: u64, app_id: &str, title: &str) -> WindowInfo {
        WindowInfo {
            id,
            app_id: Some(app_id.to_string()),
            title: Some(title.to_string()),
            workspace_id: None,
            floating: false,
            size: None,
            position: None,
        }
    }

    #[test]
    fn test_filter_windows_matches_case_insensitively_and_sorts() {
        let windows = vec![
            window(9, "Zen", "Inbox"),
            window(3, "foot", "Terminal"),
            window(5, "firefox", "docs"),
        ];

        let sorted = filter_windows(windows.clone(), &(None, None));
        assert_eq!(
            sorted.iter().map(|w| w.id).collect::<Vec<_>>(),
            vec![3, 5, 9]
        );

        let by_app = filter_windows(windows.clone(), &(Some("FOOT".to_string()), None));
        assert_eq!(by_app.len(), 1);
        assert_eq!(by_app[0].id, 3);

        let by_title = filter_windows(windows.clone(), &(None, Some("INBOX".to_string())));
        assert_eq!(by_title.len(), 1);
        assert_eq!(by_title[0].id, 9);

        let both = filter_windows(
            windows,
            &(Some("firefox".to_string()), Some("docs".to_string())),
        );
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].id, 5);
    }

    #[test]
    fn test_filter_windows_requires_a_value_for_a_configured_filter() {
        let missing: Vec<WindowInfo> = vec![WindowInfo {
            id: 1,
            app_id: None,
            title: None,
            workspace_id: None,
            floating: false,
            size: None,
            position: None,
        }];

        let filtered = filter_windows(missing.clone(), &(Some("x".to_string()), None));
        assert!(filtered.is_empty());
        assert_eq!(filter_windows(missing, &(None, None)).len(), 1);
    }

    #[test]
    fn test_data_view_follows_the_command() {
        let cli = |args: &[&str]| Cli::try_parse_from(args).unwrap().command;
        assert_eq!(data_view(&cli(&["nsticky", "windows"])), DataView::Windows);
        assert_eq!(
            data_view(&cli(&["nsticky", "sticky", "list"])),
            DataView::Ids
        );
        assert_eq!(
            data_view(&cli(&["nsticky", "stage", "list"])),
            DataView::Ids
        );
        assert_eq!(
            data_view(&cli(&["nsticky", "sticky", "add", "1"])),
            DataView::Raw
        );
    }

    #[test]
    fn test_first_window_id_reads_selector_output_lines() {
        assert_eq!(first_window_id("42\tfirefox — Inbox"), Some(42));
        assert_eq!(first_window_id("  7"), Some(7));
        assert_eq!(first_window_id("firefox — Inbox"), None);
        assert_eq!(first_window_id(""), None);
    }

    #[test]
    fn test_cli_aliases() {
        let cli = Cli::try_parse_from(["nsticky", "sticky", "a", "5"]).unwrap();
        assert_eq!(cli.into_request(), protocol::Request::Add { window_id: 5 });
        let cli = Cli::try_parse_from(["nsticky", "sticky", "r", "3"]).unwrap();
        assert_eq!(
            cli.into_request(),
            protocol::Request::Remove { window_id: 3 }
        );
        let cli = Cli::try_parse_from(["nsticky", "sticky", "l"]).unwrap();
        assert_eq!(cli.into_request(), protocol::Request::List);
        let cli = Cli::try_parse_from(["nsticky", "sticky", "t"]).unwrap();
        assert_eq!(cli.into_request(), protocol::Request::ToggleActive);
    }

    /// A window whose app id and title niri does not report.
    fn anonymous(id: u64) -> WindowInfo {
        WindowInfo {
            id,
            app_id: None,
            title: None,
            workspace_id: None,
            floating: false,
            size: None,
            position: None,
        }
    }

    #[tokio::test]
    async fn test_windows_table_aligns_columns_and_marks_missing_fields() {
        let dir = Dir::new();
        let long_title = "a window title long enough to run past the title column";
        let _daemon = FakeDaemon::start(
            &dir,
            vec![windows_data(&[
                window(7, "foot", long_title),
                anonymous(12),
            ])],
        )
        .await;

        let (result, out, _) = run(&dir.endpoints(), &["nsticky", "windows"]).await;
        result.unwrap();

        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert_eq!(lines[0].find("ID"), Some(0));
        assert_eq!(lines[0].find("APP_ID"), Some(11));
        assert_eq!(lines[0].find("TITLE"), Some(37));
        assert_eq!(lines[1].find("7"), Some(0));
        assert_eq!(lines[1].find("foot"), Some(11));
        assert_eq!(lines[1].find(long_title), Some(37), "{out}");
        assert_eq!(lines[2].find("12"), Some(0));
        assert_eq!(lines[2].find("<unknown>"), Some(11), "{out}");
        assert_eq!(lines[2].rfind("<unknown>"), Some(37), "{out}");
    }

    #[tokio::test]
    async fn test_windows_table_with_no_windows_prints_the_header_only() {
        let dir = Dir::new();
        let _daemon = FakeDaemon::start(&dir, vec![windows_data(&[])]).await;

        let (result, out, _) = run(&dir.endpoints(), &["nsticky", "windows"]).await;
        result.unwrap();
        assert_eq!(
            out, "ID         APP_ID                    TITLE\n",
            "an empty list still prints the columns"
        );
    }

    #[tokio::test]
    async fn test_windows_json_filters_by_app_id_and_sorts_by_id() {
        let dir = Dir::new();
        let _daemon = FakeDaemon::start(
            &dir,
            vec![windows_data(&[
                window(9, "zen", "Inbox"),
                window(3, "Firefox", "Docs"),
                window(5, "firefox", "Terminal"),
            ])],
        )
        .await;

        let (result, out, _) = run(
            &dir.endpoints(),
            &["nsticky", "windows", "--json", "--app-id", "FIRE"],
        )
        .await;
        result.unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON on stdout");
        let ids: Vec<u64> = parsed
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["id"].as_u64().unwrap())
            .collect();
        assert_eq!(ids, vec![3, 5], "{out}");
        assert_eq!(parsed[0]["title"], "Docs", "whole windows are reported");
    }

    #[tokio::test]
    async fn test_sticky_list_joins_the_ids_with_window_details() {
        let dir = Dir::new();
        let daemon = FakeDaemon::start(
            &dir,
            vec![
                ids_data(&[9, 3, 4]),
                windows_data(&[window(9, "zen", "Inbox"), window(3, "foot", "Terminal")]),
            ],
        )
        .await;

        let (result, out, _) = run(&dir.endpoints(), &["nsticky", "sticky", "list"]).await;
        result.unwrap();

        assert_eq!(
            daemon.seen(),
            vec![protocol::Request::List, protocol::Request::Windows],
            "the table needs the window list the id-only reply lacks"
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert!(
            lines[1].starts_with("3 ") && lines[1].contains("Terminal"),
            "{out}"
        );
        assert!(
            lines[2].starts_with("9 ") && lines[2].contains("Inbox"),
            "{out}"
        );
        assert!(
            !out.contains(" 4 "),
            "an id niri no longer has is skipped: {out}"
        );
    }

    #[tokio::test]
    async fn test_stage_list_json_needs_no_second_round_trip() {
        let dir = Dir::new();
        let daemon = FakeDaemon::start(&dir, vec![ids_data(&[5, 3])]).await;

        let (result, out, _) = run(&dir.endpoints(), &["nsticky", "stage", "list", "--json"]).await;
        result.unwrap();

        assert_eq!(daemon.seen(), vec![protocol::Request::StageList]);
        assert_eq!(serde_json::from_str::<Vec<u64>>(&out).unwrap(), vec![5, 3]);
    }

    #[tokio::test]
    async fn test_list_rejects_a_data_reply_that_is_not_an_id_array() {
        let dir = Dir::new();
        let _daemon =
            FakeDaemon::start(&dir, vec![protocol::Response::data(r#"{"nope":true}"#)]).await;

        let (result, _, _) = run(&dir.endpoints(), &["nsticky", "sticky", "list"]).await;
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("Failed to parse id list"), "{message}");
    }

    #[tokio::test]
    async fn test_success_response_prints_the_daemon_message() {
        let dir = Dir::new();
        let daemon = FakeDaemon::start(
            &dir,
            vec![protocol::Response::success("Added window 42 to sticky")],
        )
        .await;

        let (result, out, err) = run(&dir.endpoints(), &["nsticky", "sticky", "add", "42"]).await;
        result.unwrap();

        assert_eq!(out, "Added window 42 to sticky\n");
        assert!(err.is_empty(), "{err}");
        assert_eq!(
            daemon.seen(),
            vec![protocol::Request::Add { window_id: 42 }],
            "the parsed command is what goes over the socket"
        );
    }

    #[tokio::test]
    async fn test_daemon_error_response_fails_the_command() {
        let dir = Dir::new();
        let _daemon = FakeDaemon::start(
            &dir,
            vec![protocol::Response::error("No window is focused")],
        )
        .await;

        let (result, out, _) = run(&dir.endpoints(), &["nsticky", "stage", "toggle-active"]).await;
        let message = format!("{:#}", result.unwrap_err());
        assert_eq!(message, "No window is focused");
        assert!(out.is_empty(), "a failed command prints nothing: {out}");
    }

    #[tokio::test]
    async fn test_malformed_daemon_reply_is_an_error() {
        let dir = Dir::new();
        let _daemon = FakeDaemon::raw(&dir, vec![r#"{"status":"#.to_string()]).await;

        let (result, _, _) = run(&dir.endpoints(), &["nsticky", "reload"]).await;
        let message = format!("{:#}", result.unwrap_err());
        assert!(
            message.contains("Invalid JSON response from daemon"),
            "{message}"
        );
    }

    #[test]
    fn test_config_check_prints_the_parsed_configuration() {
        let dir = Dir::new();
        let config = dir.file("config.toml", FULL_CONFIG);
        let endpoints = Endpoints {
            config: config.clone(),
            ..dir.endpoints()
        };

        let mut out = Vec::new();
        run_config_check(&endpoints, false, &mut out).unwrap();

        let text = String::from_utf8(out).unwrap();
        let config_line = format!("config:          {}", config.display());
        let state_line = format!("state:           {}", endpoints.state.display());
        assert_eq!(
            text.lines().map(str::trim_end).collect::<Vec<_>>(),
            vec![
                config_line.as_str(),
                state_line.as_str(),
                "stage workspace: parking (kept while empty: true)",
                "scratchpad:      scratchpad (windows parked there)",
                "sticky follow:   own-output",
                "menu:            rofi -dmenu",
                "scratchpads:",
                "  term     60%x900px    floating spawn: foot -a nsticky-term",
                "rules:",
                "  sticky discord  app-id: 2, title: 1, exclude-title: 2",
                "  stage  games  app-id: 1, exclude-app-id: 1",
            ],
            "{text}"
        );
    }

    #[test]
    fn test_config_check_json_has_the_documented_shape() {
        let dir = Dir::new();
        let config = dir.file("config.toml", FULL_CONFIG);
        let endpoints = Endpoints {
            config: config.clone(),
            ..dir.endpoints()
        };

        let mut out = Vec::new();
        run_config_check(&endpoints, true, &mut out).unwrap();
        let report: serde_json::Value = serde_json::from_slice(&out).unwrap();

        /// Sorted keys, so the shape is compared and not serde's field order.
        fn keys(value: &serde_json::Value) -> Vec<&str> {
            let mut keys: Vec<&str> = value
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort();
            keys
        }
        assert_eq!(
            keys(&report),
            vec![
                "config",
                "menu",
                "rules",
                "scratchpad_workspace",
                "scratchpads",
                "stage_keep_workspace",
                "stage_workspace",
                "state",
                "sticky_follow",
            ]
        );
        assert_eq!(report["config"], config.display().to_string());
        assert_eq!(report["state"], endpoints.state.display().to_string());
        assert_eq!(report["stage_workspace"], "parking");
        assert_eq!(report["stage_keep_workspace"], true);
        assert_eq!(report["scratchpad_workspace"], "scratchpad");
        assert_eq!(report["sticky_follow"], "own-output");
        assert_eq!(report["menu"], "rofi -dmenu");

        assert_eq!(
            keys(&report["rules"][0]),
            vec![
                "action",
                "app_id",
                "exclude_app_id",
                "exclude_title",
                "id",
                "outputs",
                "title",
            ]
        );
        assert_eq!(report["rules"][0]["action"], "sticky");
        assert_eq!(report["rules"][0]["id"], "sticky.discord");
        assert_eq!(report["rules"][0]["app_id"], 2);
        assert_eq!(report["rules"][0]["title"], 1);
        assert_eq!(report["rules"][0]["exclude_app_id"], 0);
        assert_eq!(report["rules"][0]["exclude_title"], 2);
        assert_eq!(report["rules"][0]["outputs"], serde_json::json!([]));
        assert_eq!(report["rules"][1]["action"], "stage");
        assert_eq!(report["rules"][1]["id"], "stage.games");

        assert_eq!(
            keys(&report["scratchpads"][0]),
            vec!["float", "name", "size", "spawn"]
        );
        assert_eq!(report["scratchpads"][0]["name"], "term");
        assert_eq!(report["scratchpads"][0]["size"], "60%x900px");
        assert_eq!(report["scratchpads"][0]["float"], true);
        assert_eq!(
            report["scratchpads"][0]["spawn"],
            serde_json::json!(["foot", "-a", "nsticky-term"])
        );
    }

    #[test]
    fn test_config_check_reports_a_bad_regex_with_the_rule_and_field() {
        let dir = Dir::new();
        let config = dir.file("config.toml", "[sticky.discord]\napp-id = \"(\"\n");
        let endpoints = Endpoints {
            config: config.clone(),
            ..dir.endpoints()
        };

        let mut out = Vec::new();
        let message = format!(
            "{:#}",
            run_config_check(&endpoints, false, &mut out).unwrap_err()
        );
        assert!(message.contains("sticky.discord"), "{message}");
        assert!(message.contains("app-id"), "{message}");
        assert!(
            message.contains(&config.display().to_string()),
            "the failing file is named: {message}"
        );
        assert!(
            out.is_empty(),
            "nothing is printed for a config that does not load"
        );
    }

    #[test]
    fn test_config_check_reports_a_config_that_is_not_toml() {
        let dir = Dir::new();
        let config = dir.file("config.toml", "stage-workspace =\n");
        let endpoints = Endpoints {
            config,
            ..dir.endpoints()
        };

        let mut out = Vec::new();
        let message = format!(
            "{:#}",
            run_config_check(&endpoints, false, &mut out).unwrap_err()
        );
        assert!(message.contains("Failed to parse TOML"), "{message}");
        assert!(out.is_empty());
    }

    #[test]
    fn test_config_check_reports_an_unreadable_config() {
        let dir = Dir::new();
        // The default path, never written.
        let endpoints = dir.endpoints();

        let mut out = Vec::new();
        let message = format!(
            "{:#}",
            run_config_check(&endpoints, false, &mut out).unwrap_err()
        );
        assert!(message.contains("Failed to read config"), "{message}");
        assert!(message.contains("config.toml"), "{message}");
    }

    #[test]
    fn test_config_check_json_drops_a_rule_without_a_positive_field() {
        let dir = Dir::new();
        let config = dir.file(
            "config.toml",
            "[sticky.solitaire]\nexclude-title = \"Klondike\"\n",
        );
        let endpoints = Endpoints {
            config,
            ..dir.endpoints()
        };

        let mut out = Vec::new();
        run_config_check(&endpoints, true, &mut out).unwrap();
        let report: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            report["rules"],
            serde_json::json!([]),
            "an exclusion on its own would match every window"
        );
    }

    #[tokio::test]
    async fn test_status_prints_the_daemon_and_compositor_state() {
        let dir = Dir::new();
        let endpoints = dir.endpoints();
        let daemon = FakeDaemon::start(
            &dir,
            vec![
                ids_data(&[1, 2, 3]),
                ids_data(&[7]),
                windows_data(&[window(1, "foot", "Terminal")]),
            ],
        )
        .await;

        let (result, out, _) = run(&endpoints, &["nsticky", "status"]).await;
        result.unwrap();

        assert_eq!(
            out,
            format!(
                "socket: {}\nconfig: {}\nstate:  {}\ndaemon: running\n\
                 sticky: 3 window(s)\nstaged: 1 window(s)\nniri:   connected\n",
                endpoints.socket.display(),
                endpoints.config.display(),
                endpoints.state.display()
            )
        );
        assert_eq!(
            daemon.seen(),
            vec![
                protocol::Request::List,
                protocol::Request::StageList,
                protocol::Request::Windows
            ]
        );
    }

    #[tokio::test]
    async fn test_status_reports_an_unreachable_compositor() {
        let dir = Dir::new();
        let _daemon = FakeDaemon::start(
            &dir,
            vec![
                protocol::Response::data("[]"),
                protocol::Response::error("staged list failed"),
                protocol::Response::error("NIRI_SOCKET is not set"),
            ],
        )
        .await;

        let (result, out, _) = run(&dir.endpoints(), &["nsticky", "status"]).await;
        result.unwrap();

        assert!(out.contains("daemon: running"), "{out}");
        assert!(out.contains("sticky: 0 window(s)"), "{out}");
        assert!(
            out.contains("staged: 0 window(s)"),
            "a staged list the daemon could not answer counts as none: {out}"
        );
        assert!(
            out.contains("niri:   unreachable (NIRI_SOCKET is not set)"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn test_status_fails_fast_when_no_daemon_is_listening() {
        let dir = Dir::new();
        let endpoints = dir.endpoints();

        let (result, out, _) = run(&endpoints, &["nsticky", "status"]).await;

        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            "The nsticky daemon is not running"
        );
        assert!(out.contains("daemon: not running"), "{out}");
        assert!(
            out.contains(&endpoints.socket.display().to_string()),
            "the report still says which socket was tried: {out}"
        );
    }

    #[tokio::test]
    async fn test_status_treats_a_malformed_reply_as_no_daemon() {
        let dir = Dir::new();
        let _daemon = FakeDaemon::raw(&dir, vec![r#"{"status":"#.to_string()]).await;

        let (result, out, _) = run(&dir.endpoints(), &["nsticky", "status"]).await;
        assert!(result.is_err());
        assert!(out.contains("daemon: not running"), "{out}");
    }

    #[test]
    fn test_unknown_subcommand_is_a_usage_error() {
        let error = Cli::try_parse_from(["nsticky", "frobnicate"]).unwrap_err();
        assert_ne!(error.exit_code(), 0);
        assert!(error.to_string().contains("frobnicate"), "{error}");
    }

    #[test]
    fn test_missing_arguments_are_usage_errors() {
        for (args, missing) in [
            (["nsticky", "sticky", "add"], "WINDOW_ID"),
            (["nsticky", "sticky", "remove"], "WINDOW_ID"),
            (["nsticky", "stage", "toggle-appid"], "APPID"),
            (["nsticky", "stage", "toggle-title"], "TITLE"),
        ] {
            let error = Cli::try_parse_from(args).unwrap_err();
            assert_ne!(error.exit_code(), 0, "{args:?}");
            assert!(
                matches!(
                    error.kind(),
                    clap::error::ErrorKind::MissingRequiredArgument
                ),
                "{args:?} -> {error}"
            );
            assert!(error.to_string().contains(missing), "{args:?} -> {error}");
        }
    }

    #[test]
    fn test_a_non_numeric_window_id_is_a_usage_error() {
        let error = Cli::try_parse_from(["nsticky", "sticky", "add", "abc"]).unwrap_err();
        assert_ne!(error.exit_code(), 0);
        assert!(
            matches!(error.kind(), clap::error::ErrorKind::ValueValidation),
            "{error}"
        );
        assert!(error.to_string().contains("abc"), "{error}");
    }

    #[test]
    fn test_toggle_appid_and_toggle_title_stay_distinct() {
        for (args, expected) in [
            (
                ["nsticky", "sticky", "ta", "firefox"],
                protocol::Request::ToggleAppid {
                    appid: "firefox".to_string(),
                },
            ),
            (
                ["nsticky", "sticky", "tt", "Gmail"],
                protocol::Request::ToggleTitle {
                    title: "Gmail".to_string(),
                },
            ),
            (
                ["nsticky", "stage", "ta", "chromium"],
                protocol::Request::StageToggleAppid {
                    appid: "chromium".to_string(),
                },
            ),
            (
                ["nsticky", "stage", "tt", "Terminal"],
                protocol::Request::StageToggleTitle {
                    title: "Terminal".to_string(),
                },
            ),
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert_eq!(cli.into_request(), expected, "{args:?}");
        }
    }

    #[test]
    fn test_json_flag_is_accepted_on_the_list_commands() {
        // `--json` is global, so it must work on either side of the subcommand;
        // the list commands are the ones whose output it changes.
        for args in [
            vec!["nsticky", "windows", "--json"],
            vec!["nsticky", "--json", "windows"],
            vec!["nsticky", "sticky", "list", "--json"],
            vec!["nsticky", "--json", "sticky", "list"],
            vec!["nsticky", "stage", "list", "--json"],
        ] {
            let cli = Cli::try_parse_from(&args).unwrap();
            assert!(cli.json, "{args:?}");
            assert_ne!(data_view(&cli.command), DataView::Raw, "{args:?}");
        }
    }

    #[test]
    fn test_json_flag_parses_on_the_commands_that_ignore_it() {
        for args in [
            vec!["nsticky", "status", "--json"],
            vec!["nsticky", "reload", "--json"],
            vec!["nsticky", "config", "check", "--json"],
            vec!["nsticky", "stage", "restore", "--json"],
            vec!["nsticky", "scratchpad", "--json"],
            vec!["nsticky", "sticky", "add", "1", "--json"],
        ] {
            let cli = Cli::try_parse_from(&args).unwrap();
            assert!(cli.json, "{args:?}");
        }
    }

    #[tokio::test]
    async fn test_stage_restore_without_a_selector_says_what_to_configure() {
        let dir = Dir::new();
        let daemon = FakeDaemon::start(&dir, vec![ids_data(&[5])]).await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run_stage_restore(
            &dir.endpoints(),
            RestoreEnv {
                menu: None,
                from_env: None,
                stdin_is_terminal: false,
                available: |_| false,
            },
            &mut std::io::Cursor::new(String::new()),
            &mut out,
            &mut err,
        )
        .await;

        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("No selector available"), "{message}");
        assert!(message.contains("vicinae"), "{message}");
        assert!(message.contains("rofi"), "{message}");
        assert!(message.contains("NSTICKY_MENU"), "{message}");
        assert!(out.is_empty(), "nothing is restored");
        assert_eq!(daemon.seen(), vec![protocol::Request::StageList]);
    }

    #[tokio::test]
    async fn test_stage_restore_terminal_reports_an_empty_stage() {
        let dir = Dir::new();
        let daemon = FakeDaemon::start(&dir, vec![ids_data(&[])]).await;

        let (result, out, err) = terminal_restore(&dir, "").await;
        result.unwrap();

        assert_eq!(out, "No staged windows.\n");
        assert!(err.is_empty(), "{err}");
        assert_eq!(
            daemon.seen(),
            vec![protocol::Request::StageList],
            "an empty stage prompts for nothing"
        );
    }

    #[tokio::test]
    async fn test_stage_restore_terminal_reports_windows_that_vanished() {
        let dir = Dir::new();
        let daemon = FakeDaemon::start(
            &dir,
            vec![
                ids_data(&[5]),
                windows_data(&[window(7, "foot", "Terminal")]),
            ],
        )
        .await;

        let (result, out, _) = terminal_restore(&dir, "1\n").await;
        result.unwrap();

        assert_eq!(out, "No active staged windows found.\n");
        assert_eq!(
            daemon.seen(),
            vec![protocol::Request::StageList, protocol::Request::Windows],
            "no unstage is sent for a window niri does not have"
        );
    }

    #[tokio::test]
    async fn test_stage_restore_terminal_restores_the_typed_numbers() {
        let dir = Dir::new();
        let daemon = FakeDaemon::start(
            &dir,
            vec![
                ids_data(&[5, 7]),
                windows_data(&[window(5, "foot", "Terminal"), window(7, "zen", "Inbox")]),
                protocol::Response::success("Unstaged window 7"),
                protocol::Response::success("Unstaged window 5"),
            ],
        )
        .await;

        let (result, out, _) = terminal_restore(&dir, " 2 zz 99 0 1 \n").await;
        result.unwrap();

        assert_eq!(
            daemon.seen(),
            vec![
                protocol::Request::StageList,
                protocol::Request::Windows,
                protocol::Request::Unstage { window_id: 7 },
                protocol::Request::Unstage { window_id: 5 },
            ],
            "numbers are the prompt's 1-based positions; junk and out-of-range are skipped"
        );
        assert!(out.contains(" 1. foot — Terminal  [ID: 5]"), "{out}");
        assert!(out.contains(" 2. zen — Inbox  [ID: 7]"), "{out}");
        assert!(
            out.contains("Restore (1-2, space-separated for multiple, q): "),
            "{out}"
        );
        assert!(out.contains("Unstaged window 7"), "{out}");
        assert!(out.contains("Unstaged window 5"), "{out}");
    }

    #[tokio::test]
    async fn test_stage_restore_terminal_cancels_on_q_and_on_an_empty_line() {
        for typed in ["q\n", "Q\n", "\n"] {
            let dir = Dir::new();
            let daemon = FakeDaemon::start(
                &dir,
                vec![
                    ids_data(&[5]),
                    windows_data(&[window(5, "foot", "Terminal")]),
                ],
            )
            .await;

            let (result, out, _) = terminal_restore(&dir, typed).await;
            result.unwrap();

            assert_eq!(
                daemon.seen().len(),
                2,
                "nothing is unstaged for {typed:?}: {out}"
            );
        }
    }

    #[tokio::test]
    async fn test_stage_restore_menu_restores_what_the_selector_prints() {
        let dir = Dir::new();
        let dump = dir.path().join("selector-stdin");
        let selector = dir.script(
            "selector",
            &format!(
                "#!/bin/sh\nwhile IFS= read -r line || [ -n \"$line\" ]; do printf '%s\\n' \"$line\" >> \"{}\"; done\necho 7\n",
                dump.display()
            ),
        );
        let daemon = FakeDaemon::start(
            &dir,
            vec![
                ids_data(&[5, 7]),
                windows_data(&[window(5, "foot", "Terminal"), window(7, "zen", "Inbox")]),
                protocol::Response::success("Unstaged window 7"),
            ],
        )
        .await;

        let (result, out, err) =
            menu_restore(&dir, MenuSpec::Args(vec![selector.display().to_string()])).await;
        result.unwrap();

        assert_eq!(
            std::fs::read_to_string(&dump)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            vec!["5\tfoot — Terminal", "7\tzen — Inbox"],
            "the selector is fed one id-first line per staged window"
        );
        assert_eq!(
            daemon.seen().last(),
            Some(&protocol::Request::Unstage { window_id: 7 }),
            "the id the selector printed is the window that gets unstaged"
        );
        assert!(out.contains("Unstaged window 7"), "{out}");
        assert!(err.is_empty(), "{err}");
    }

    #[tokio::test]
    async fn test_stage_restore_opens_the_selector_even_with_nothing_staged() {
        let dir = Dir::new();
        let selector = dir.script("selector", "#!/bin/sh\nexit 3\n");
        let daemon = FakeDaemon::start(&dir, vec![ids_data(&[]), windows_data(&[])]).await;

        let (result, out, err) =
            menu_restore(&dir, MenuSpec::Args(vec![selector.display().to_string()])).await;
        result.unwrap();

        assert_eq!(
            daemon.seen(),
            vec![protocol::Request::StageList, protocol::Request::Windows],
            "the selector is opened so its empty state is visible"
        );
        assert!(out.is_empty(), "{out}");
        assert!(
            err.contains("Menu exited with exit status: 3"),
            "a selector that fails is reported: {err}"
        );
        assert!(err.contains("selector"), "and so is the command: {err}");
    }

    #[tokio::test]
    async fn test_stage_restore_reports_a_selector_that_cannot_be_launched() {
        let dir = Dir::new();
        let _daemon = FakeDaemon::start(&dir, vec![ids_data(&[]), windows_data(&[])]).await;

        let (result, _, _) = menu_restore(
            &dir,
            MenuSpec::CommandLine("/nonexistent/nsticky-selector --dmenu".to_string()),
        )
        .await;

        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("Failed to launch menu"), "{message}");
        assert!(
            message.contains("/nonexistent/nsticky-selector"),
            "{message}"
        );
    }

    #[test]
    fn test_config_check_prints_the_defaults_of_an_empty_config() {
        let dir = Dir::new();
        let config = dir.file("config.toml", "");
        let endpoints = Endpoints {
            config: config.clone(),
            ..dir.endpoints()
        };

        let mut out = Vec::new();
        run_config_check(&endpoints, false, &mut out).unwrap();

        let text = String::from_utf8(out).unwrap();
        let config_line = format!("config:          {}", config.display());
        let state_line = format!("state:           {}", endpoints.state.display());
        assert_eq!(
            text.lines().map(str::trim_end).collect::<Vec<_>>(),
            vec![
                config_line.as_str(),
                state_line.as_str(),
                "stage workspace: stage (kept while empty: false)",
                "scratchpad:      scratchpad (windows parked there)",
                "sticky follow:   focused",
                "menu:            (none, `stage restore` falls back to the terminal)",
                "scratchpads:     (none)",
                "rules:           (none)",
            ],
            "{text}"
        );
    }

    #[test]
    fn test_config_check_lists_a_scratchpad_with_no_spawn_command() {
        let dir = Dir::new();
        dir.file("config.toml", "[scratchpad.plain]\nfloat = false\n");
        let endpoints = dir.endpoints();

        let mut out = Vec::new();
        run_config_check(&endpoints, false, &mut out).unwrap();

        let text = String::from_utf8(out).unwrap();
        let row = text
            .lines()
            .find(|line| line.trim_start().starts_with("plain"))
            .unwrap_or_else(|| panic!("no scratchpad row in:\n{text}"));
        assert_eq!(row.find("plain"), Some(2), "{row}");
        assert_eq!(row.find("autoxauto"), Some(11), "{row}");
        assert_eq!(row.find("tiled"), Some(24), "{row}");
        assert_eq!(row.find("(no spawn command)"), Some(33), "{row}");
    }

    #[tokio::test]
    async fn test_list_reports_a_window_lookup_that_failed() {
        let dir = Dir::new();
        let daemon = FakeDaemon::start(
            &dir,
            vec![
                ids_data(&[5]),
                protocol::Response::error("niri is not reachable"),
            ],
        )
        .await;

        let (result, out, _) = run(&dir.endpoints(), &["nsticky", "sticky", "list"]).await;

        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            "niri is not reachable"
        );
        assert!(out.is_empty(), "no table without the window details: {out}");
        assert_eq!(
            daemon.seen(),
            vec![protocol::Request::List, protocol::Request::Windows]
        );
    }

    #[tokio::test]
    async fn test_stage_restore_reports_a_stage_list_that_failed() {
        let dir = Dir::new();
        let launched = dir.path().join("selector-ran");
        let selector = dir.script(
            "selector",
            &format!("#!/bin/sh\ntouch \"{}\"\n", launched.display()),
        );
        let _daemon = FakeDaemon::start(
            &dir,
            vec![protocol::Response::error("niri is not reachable")],
        )
        .await;

        let (result, out, err) =
            menu_restore(&dir, MenuSpec::Args(vec![selector.display().to_string()])).await;

        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            "niri is not reachable"
        );
        assert!(out.is_empty() && err.is_empty(), "{out}{err}");
        assert!(
            !launched.exists(),
            "the selector is not launched when the staged list cannot be read"
        );
    }

    #[tokio::test]
    async fn test_stage_restore_terminal_stops_when_an_unstage_is_refused() {
        let dir = Dir::new();
        let daemon = FakeDaemon::start(
            &dir,
            vec![
                ids_data(&[5, 7]),
                windows_data(&[window(5, "foot", "Terminal"), window(7, "zen", "Inbox")]),
                protocol::Response::success("Unstaged window 5"),
                protocol::Response::error("Window 7 is gone"),
            ],
        )
        .await;

        let (result, out, _) = terminal_restore(&dir, "1 2\n").await;

        assert_eq!(format!("{:#}", result.unwrap_err()), "Window 7 is gone");
        assert!(
            out.contains("Unstaged window 5"),
            "what already happened stays reported: {out}"
        );
        assert_eq!(
            daemon.seen().len(),
            4,
            "the refused window is not retried and the rest is not attempted"
        );
    }

    #[tokio::test]
    async fn test_status_reports_a_compositor_probe_that_answered_badly() {
        // The window-list probe answers with a message instead of a list: the
        // report says so rather than claiming niri is connected.
        let dir = Dir::new();
        let _daemon = FakeDaemon::start(
            &dir,
            vec![
                ids_data(&[]),
                ids_data(&[]),
                protocol::Response::success("handled"),
            ],
        )
        .await;
        let (result, out, _) = run(&dir.endpoints(), &["nsticky", "status"]).await;
        result.unwrap();
        assert!(
            out.contains(
                r#"niri:   unreachable (unexpected response: Success { message: "handled" })"#
            ),
            "{out}"
        );

        // And one that stops speaking the protocol mid-report.
        let dir = Dir::new();
        let _daemon = FakeDaemon::raw(
            &dir,
            vec![
                serde_json::to_string(&ids_data(&[])).unwrap(),
                serde_json::to_string(&ids_data(&[])).unwrap(),
                "{".to_string(),
            ],
        )
        .await;
        let (result, out, _) = run(&dir.endpoints(), &["nsticky", "status"]).await;
        result.unwrap();
        assert!(
            out.contains("niri:   unreachable (Invalid JSON response from daemon"),
            "{out}"
        );
    }
}
