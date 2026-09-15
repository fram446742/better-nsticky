//! External selector command for `stage restore`: parsing and launching.
//!
//! Two configuration shapes converge on [`MenuCommand`]:
//!
//! - `menu = "vicinae dmenu --placeholder 'Restore Window:'"` — split with
//!   shell-style quoting rules ([`shell_words`]).
//! - `menu = ["vicinae", "dmenu", "--placeholder", "Restore Window:"]` — argv
//!   as written, no parsing.
//!
//! `NSTICKY_MENU` goes through the same parser as the string form, and nothing
//! is ever handed to a shell. Vicinae (<https://www.vicinae.com/>) is the
//! recommended selector.

use anyhow::{Context, Result, anyhow, bail};
use std::ffi::OsStr;
use std::process::{Child, Command, Stdio};

/// Selector to use for `stage restore`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// A configured or discovered selector.
    Command(MenuSpec),
    /// Nothing configured, but stdin is a terminal: the built-in prompt works.
    Terminal,
    /// Nothing configured and stdin is not a terminal, so no prompt can be
    /// shown and no selector was found.
    Unavailable,
}

/// Selectors tried in order when `stage restore` has no configured menu and no
/// terminal.
pub const SELECTOR_CANDIDATES: [(&str, &[&str]); 5] = [
    ("vicinae", &["dmenu", "--placeholder", "Restore Window:"]),
    ("rofi", &["-dmenu", "-p", "Restore Window:"]),
    ("fuzzel", &["--dmenu", "--prompt", "Restore Window:"]),
    ("wofi", &["--show", "dmenu", "--prompt", "Restore Window:"]),
    ("pantry", &["-m"]),
];

/// Resolve the selector for `stage restore`.
///
/// `NSTICKY_MENU` wins over the `menu` config key. The `PATH` search only runs
/// when stdin is not a terminal, where the built-in prompt is useless.
pub fn resolve_selector(
    config: Option<MenuSpec>,
    from_env: Option<MenuSpec>,
    stdin_is_terminal: bool,
    available: impl Fn(&str) -> bool,
) -> Selector {
    if let Some(spec) = from_env.or(config) {
        return Selector::Command(spec);
    }

    if stdin_is_terminal {
        return Selector::Terminal;
    }

    for (program, args) in SELECTOR_CANDIDATES {
        if available(program) {
            return Selector::Command(MenuSpec::Args(
                std::iter::once(program)
                    .chain(args.iter().copied())
                    .map(String::from)
                    .collect(),
            ));
        }
    }

    Selector::Unavailable
}

/// Whether `program` is an executable file on `PATH`.
pub fn is_available(program: &str) -> bool {
    is_available_in(program, std::env::var_os("PATH").as_deref())
}

/// The lookup itself, with `PATH` passed in so tests can supply their own.
fn is_available_in(program: &str, path: Option<&OsStr>) -> bool {
    let Some(path) = path else {
        return false;
    };

    std::env::split_paths(path).any(|dir| {
        let candidate = dir.join(program);
        std::fs::metadata(&candidate)
            .map(|meta| meta.is_file() && is_executable(&meta))
            .unwrap_or(false)
    })
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    true
}

/// Raw `menu` value as read from the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuSpec {
    /// A command line that needs shell-style word splitting.
    CommandLine(String),
    /// An explicit argv, used as-is.
    Args(Vec<String>),
}

/// A selector command split into an executable and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuCommand {
    program: String,
    args: Vec<String>,
    /// Original spec, kept for error messages.
    origin: String,
}

impl MenuSpec {
    /// The `NSTICKY_MENU` environment variable, if it is set and not blank.
    pub fn from_env() -> Option<Self> {
        Self::from_value(std::env::var("NSTICKY_MENU").ok())
    }

    /// A raw `NSTICKY_MENU` value: blank counts as unset. Taken as a parameter
    /// so tests stay out of the environment.
    fn from_value(value: Option<String>) -> Option<Self> {
        value
            .filter(|value| !value.trim().is_empty())
            .map(Self::CommandLine)
    }

    /// The spec as written, for display in `config check`.
    pub fn describe(&self) -> String {
        match self {
            MenuSpec::CommandLine(command) => command.clone(),
            MenuSpec::Args(args) => shell_words::join(args),
        }
    }

    /// Split the spec into a program and its arguments.
    pub fn parse(&self) -> Result<MenuCommand> {
        match self {
            MenuSpec::CommandLine(cmd) => {
                let parts = shell_words::split(cmd)
                    .with_context(|| format!("invalid menu command syntax: {cmd:?}"))?;
                let origin = cmd.clone();
                MenuCommand::from_parts(parts, origin)
            }
            MenuSpec::Args(args) => MenuCommand::from_parts(args.clone(), shell_words::join(args)),
        }
    }
}

impl MenuCommand {
    fn from_parts(parts: Vec<String>, origin: String) -> Result<Self> {
        let Some((program, args)) = parts.split_first() else {
            bail!("menu command is empty");
        };
        if program.is_empty() {
            bail!("menu command has an empty program name: {origin:?}");
        }
        Ok(Self {
            program: program.clone(),
            args: args.to_vec(),
            origin,
        })
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Multi-line description used in launch errors.
    pub fn describe(&self) -> String {
        format!(
            "command:\n    {}\n\nprogram:\n    {}\n\narguments:\n    {:?}",
            self.origin,
            self.program(),
            self.args()
        )
    }

    /// Spawn the selector with the window list on its stdin and its stdout
    /// piped back; stderr is inherited.
    pub fn spawn(&self) -> Result<Child> {
        Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| {
                anyhow!(
                    "Failed to launch menu:\n\n{}\n\nerror:\n    {e}",
                    self.describe()
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(cmd: &str) -> Result<MenuCommand> {
        MenuSpec::CommandLine(cmd.to_string()).parse()
    }

    #[test]
    fn test_selector_prefers_the_environment_then_the_config() {
        let env = MenuSpec::CommandLine("rofi -dmenu".to_string());
        let config = MenuSpec::CommandLine("fuzzel --dmenu".to_string());

        assert_eq!(
            resolve_selector(Some(config.clone()), Some(env.clone()), true, |_| false),
            Selector::Command(env)
        );
        assert_eq!(
            resolve_selector(Some(config.clone()), None, true, |_| false),
            Selector::Command(config)
        );
    }

    #[test]
    fn test_selector_falls_back_to_the_terminal_prompt_without_a_tty() {
        assert_eq!(
            resolve_selector(None, None, true, |program| program == "rofi"),
            Selector::Terminal
        );
    }

    #[test]
    fn test_selector_is_discovered_when_there_is_no_terminal() {
        let selector = resolve_selector(None, None, false, |program| program == "fuzzel");
        match selector {
            Selector::Command(spec) => {
                assert_eq!(spec.describe(), "fuzzel --dmenu --prompt 'Restore Window:'");
            }
            other => panic!("expected a discovered selector, got {other:?}"),
        }
    }

    #[test]
    fn test_selector_candidates_are_tried_in_order() {
        let first = resolve_selector(None, None, false, |_| true);
        match first {
            Selector::Command(spec) => {
                assert_eq!(spec.parse().unwrap().program(), "vicinae");
            }
            other => panic!("expected a discovered selector, got {other:?}"),
        }
    }

    #[test]
    fn test_selector_is_unavailable_without_a_terminal_or_a_candidate() {
        assert_eq!(
            resolve_selector(None, None, false, |_| false),
            Selector::Unavailable
        );
    }

    #[test]
    fn test_is_available_looks_up_the_path() {
        // POSIX guarantees `sh` exists on PATH; no other name is guaranteed.
        assert!(is_available("sh"));
        assert!(!is_available("nsticky-no-such-program-8f2c"));
    }

    #[test]
    fn test_spec_describe_shows_the_original_form() {
        let command = MenuSpec::CommandLine("rofi -dmenu -p 'Pick'".to_string());
        assert_eq!(command.describe(), "rofi -dmenu -p 'Pick'");

        let args = MenuSpec::Args(vec!["rofi".into(), "pick".into()]);
        assert_eq!(args.describe(), "rofi pick");
    }

    #[test]
    fn test_plain_command() {
        let menu = parse("vicinae dmenu").unwrap();
        assert_eq!(menu.program(), "vicinae");
        assert_eq!(menu.args(), ["dmenu"]);
    }

    #[test]
    fn test_program_without_args() {
        let menu = parse("rofi").unwrap();
        assert_eq!(menu.program(), "rofi");
        assert!(menu.args().is_empty());
    }

    #[test]
    fn test_single_quoted_argument() {
        let menu = parse("vicinae dmenu --placeholder 'Restore Window:'").unwrap();
        assert_eq!(menu.program(), "vicinae");
        assert_eq!(menu.args(), ["dmenu", "--placeholder", "Restore Window:"]);
    }

    #[test]
    fn test_double_quoted_argument() {
        let menu = parse(r#"rofi -dmenu -p "Restore Window:""#).unwrap();
        assert_eq!(menu.program(), "rofi");
        assert_eq!(menu.args(), ["-dmenu", "-p", "Restore Window:"]);
    }

    #[test]
    fn test_backslash_escaped_space() {
        let menu = parse(r"fuzzel --dmenu --prompt Restore\ Window:").unwrap();
        assert_eq!(menu.program(), "fuzzel");
        assert_eq!(menu.args(), ["--dmenu", "--prompt", "Restore Window:"]);
    }

    #[test]
    fn test_backslash_escaped_quote() {
        let menu = parse(r#"wofi --show dmenu -p "Say \"hi\"""#).unwrap();
        assert_eq!(menu.program(), "wofi");
        assert_eq!(menu.args(), ["--show", "dmenu", "-p", "Say \"hi\""]);
    }

    #[test]
    fn test_leading_and_trailing_whitespace() {
        let menu = parse("   rofi   -dmenu   ").unwrap();
        assert_eq!(menu.program(), "rofi");
        assert_eq!(menu.args(), ["-dmenu"]);
    }

    #[test]
    fn test_empty_command_is_an_error() {
        let err = parse("   ").unwrap_err();
        assert!(err.to_string().contains("empty"), "unexpected error: {err}");
    }

    #[test]
    fn test_invalid_quoting_is_an_error() {
        let err = parse("rofi -p 'unterminated").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid menu command syntax"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn test_args_spec_is_used_verbatim() {
        let spec = MenuSpec::Args(vec![
            "vicinae".to_string(),
            "dmenu".to_string(),
            "--placeholder".to_string(),
            "Restore Window:".to_string(),
        ]);
        let menu = spec.parse().unwrap();
        assert_eq!(menu.program(), "vicinae");
        assert_eq!(menu.args(), ["dmenu", "--placeholder", "Restore Window:"]);
    }

    #[test]
    fn test_empty_args_spec_is_an_error() {
        let err = MenuSpec::Args(Vec::new()).parse().unwrap_err();
        assert!(err.to_string().contains("empty"), "unexpected error: {err}");
    }

    #[test]
    fn test_describe_lists_program_and_arguments() {
        let menu = parse("vicinae dmenu --placeholder 'Restore Window:'").unwrap();
        let described = menu.describe();
        assert!(
            described.contains("command:\n    vicinae dmenu"),
            "{described}"
        );
        assert!(described.contains("program:\n    vicinae"), "{described}");
        assert!(
            described.contains(r#"["dmenu", "--placeholder", "Restore Window:"]"#),
            "{described}"
        );
    }

    #[test]
    fn test_shell_metacharacters_are_not_split() {
        let menu = parse("rofi -dmenu -p 'a;b|c'").unwrap();
        assert_eq!(menu.program(), "rofi");
        assert_eq!(menu.args(), ["-dmenu", "-p", "a;b|c"]);
    }

    /// The parsed argv reaches the process verbatim: `$(id)` is not expanded.
    #[cfg(unix)]
    #[test]
    fn test_command_is_executed_directly_without_a_shell() {
        let menu = parse(r#"printf '%s' "$(id)""#).unwrap();
        assert_eq!(menu.program(), "printf");
        assert_eq!(menu.args(), ["%s", "$(id)"]);
        let output = menu.spawn().unwrap().wait_with_output().unwrap();
        assert!(output.status.success(), "{}", output.status);
        assert_eq!(String::from_utf8_lossy(&output.stdout), "$(id)");
    }

    #[test]
    fn test_launch_missing_program_reports_command_and_error() {
        let menu = parse("nsticky-no-such-selector --dmenu").unwrap();
        let err = menu.spawn().unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Failed to launch menu:"), "{msg}");
        assert!(
            msg.contains("command:\n    nsticky-no-such-selector --dmenu"),
            "{msg}"
        );
        assert!(
            msg.contains("program:\n    nsticky-no-such-selector"),
            "{msg}"
        );
        assert!(msg.contains("arguments:\n    [\"--dmenu\"]"), "{msg}");
        assert!(msg.contains("error:\n"), "{msg}");
    }

    #[test]
    fn test_empty_program_name_is_reported() {
        // `''` is a valid empty shell word, so the check, not the splitter,
        // reports the error.
        let err = MenuSpec::CommandLine("'' dmenu".to_string())
            .parse()
            .unwrap_err();
        assert!(
            err.to_string().contains("empty program name"),
            "unexpected error: {err}"
        );

        let err = MenuSpec::Args(vec![String::new(), "dmenu".to_string()])
            .parse()
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("empty program name"),
            "unexpected error: {msg}"
        );
        assert!(msg.contains("dmenu"), "the origin must be shown: {msg}");
    }

    #[test]
    fn test_quoted_empty_argument_is_kept() {
        let menu = parse("rofi -dmenu -p ''").unwrap();
        assert_eq!(menu.program(), "rofi");
        assert_eq!(menu.args(), ["-dmenu", "-p", ""]);
    }

    #[test]
    fn test_args_spec_is_not_split_at_all() {
        // Array elements are argv as written: spaces never split one.
        let spec = MenuSpec::Args(vec!["rofi -dmenu -p 'Pick'".to_string()]);
        let menu = spec.parse().unwrap();
        assert_eq!(menu.program(), "rofi -dmenu -p 'Pick'");
        assert!(menu.args().is_empty());
    }

    #[test]
    fn test_environment_value_goes_through_the_same_parser() {
        assert_eq!(MenuSpec::from_value(None), None);
        assert_eq!(MenuSpec::from_value(Some("   ".to_string())), None);

        let spec = MenuSpec::from_value(Some(
            "vicinae dmenu --placeholder 'Restore Window:'".to_string(),
        ))
        .expect("set and non-blank");
        assert_eq!(
            spec,
            MenuSpec::CommandLine("vicinae dmenu --placeholder 'Restore Window:'".to_string())
        );

        let menu = spec.parse().unwrap();
        assert_eq!(menu.program(), "vicinae");
        assert_eq!(menu.args(), ["dmenu", "--placeholder", "Restore Window:"]);
    }

    #[cfg(unix)]
    #[test]
    fn test_is_available_requires_an_executable_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("nsticky-menu-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let executable = dir.join("selector");
        let plain = dir.join("not-executable");
        std::fs::write(&executable, "#!/bin/sh\n").unwrap();
        std::fs::write(&plain, "text\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();

        let path = dir.as_os_str();
        assert!(is_available_in("selector", Some(path)));
        assert!(!is_available_in("not-executable", Some(path)));
        assert!(!is_available_in("absent", Some(path)));
        assert!(!is_available_in("selector", None));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
