use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use std::path::{Path, PathBuf};
use toml::Value;

use crate::menu::MenuSpec;

/// Keys accepted for each rule field. The hyphenated spellings are the
/// documented ones, the underscore variants exist for backwards compatibility,
/// and `not-*` are aliases of `exclude-*`. Patterns from every alias present
/// are combined, never overwritten.
const APP_ID_KEYS: [&str; 2] = ["app-id", "app_id"];
const TITLE_KEYS: [&str; 1] = ["title"];
const EXCLUDE_APP_ID_KEYS: [&str; 4] = [
    "exclude-app-id",
    "exclude_app_id",
    "not-app-id",
    "not_app_id",
];
const EXCLUDE_TITLE_KEYS: [&str; 4] = ["exclude-title", "exclude_title", "not-title", "not_title"];

/// Key matching a window that is (or is not) floating.
const FLOATING_KEYS: [&str; 1] = ["floating"];

/// Keys pinning a window to one or more outputs, in preference order.
const OUTPUT_KEYS: [&str; 2] = ["output", "outputs"];

/// Key naming the command a scratchpad starts when it is not running.
const SPAWN_KEYS: [&str; 1] = ["spawn"];
/// Key floating a scratchpad when it is shown.
const FLOAT_KEYS: [&str; 1] = ["float"];
/// Keys sizing a scratchpad when it is shown.
const WIDTH_KEYS: [&str; 1] = ["width"];
const HEIGHT_KEYS: [&str; 1] = ["height"];

/// Keys naming the workspace a scratchpad parks its window on.
const SCRATCHPAD_WORKSPACE_KEYS: [&str; 2] = ["scratchpad-workspace", "scratchpad_workspace"];

/// Workspace used for parked scratchpad windows when not configured.
const DEFAULT_SCRATCHPAD_WORKSPACE: &str = "scratchpad";

/// Key keeping the stage workspace alive while nothing is parked there.
const STAGE_KEEP_KEYS: [&str; 2] = ["stage-keep-workspace", "stage_keep_workspace"];

/// Key deciding how windows without an `output` follow workspaces.
const STICKY_FOLLOW_KEYS: [&str; 2] = ["sticky-follow", "sticky_follow"];

/// Keys accepted for the workspace staged windows are parked on.
const STAGE_WORKSPACE_KEYS: [&str; 2] = ["stage-workspace", "stage_workspace"];

/// Workspace used when `stage-workspace` is not configured.
const DEFAULT_STAGE_WORKSPACE: &str = "stage";

/// What nsticky does with a window that matches a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAction {
    /// Keep the window on every workspace (`[sticky.<name>]`).
    Sticky,
    /// Park the window on the stage workspace (`[stage.<name>]`).
    Stage,
}

/// Sections holding rules, in match precedence order.
const RULE_SECTIONS: [(WindowAction, &str); 2] = [
    (WindowAction::Sticky, "sticky"),
    (WindowAction::Stage, "stage"),
];

#[derive(Debug, Clone)]
pub struct Config {
    sticky_rules: Vec<CompiledRule>,
    stage_rules: Vec<CompiledRule>,
    menu: Option<MenuSpec>,
    stage_workspace: String,
    scratchpad_workspace: String,
    stage_keep_workspace: bool,
    sticky_follow: StickyFollow,
    scratchpads: Vec<Scratchpad>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sticky_rules: Vec::new(),
            stage_rules: Vec::new(),
            menu: None,
            stage_workspace: DEFAULT_STAGE_WORKSPACE.to_string(),
            scratchpad_workspace: DEFAULT_SCRATCHPAD_WORKSPACE.to_string(),
            stage_keep_workspace: false,
            sticky_follow: StickyFollow::default(),
            scratchpads: Vec::new(),
        }
    }
}

/// How a sticky window follows workspaces when its rule pins no output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StickyFollow {
    /// Follow the workspace that became focused, wherever it is (default).
    #[default]
    Focused,
    /// Stay on the output the window was on when it became sticky.
    OwnOutput,
}

impl StickyFollow {
    /// Name used in the configuration file.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Focused => "focused",
            Self::OwnOutput => "own-output",
        }
    }

    fn parse(value: &Value, key: &str) -> Result<Self> {
        match value.as_str() {
            Some("focused") => Ok(Self::Focused),
            Some("own-output") => Ok(Self::OwnOutput),
            Some(other) => {
                bail!("Invalid {key} value: expected \"focused\" or \"own-output\", got {other:?}")
            }
            None => bail!("Invalid {key} value: expected a string, got {value}"),
        }
    }
}

/// Size a scratchpad gets when it is shown.
///
/// niri's `SetProportion` is 0–100 (60.0 is 60%), so a percentage is written
/// `"60%"`; `0.6` is a classic mistake and is rejected.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Size {
    /// Logical pixels.
    Fixed(i32),
    /// Percentage of the working area, `0 < p <= 100`.
    Percent(f64),
}

impl Size {
    fn parse(value: &Value, key: &str, rule_id: &str) -> Result<Self> {
        let size = match value {
            Value::Integer(pixels) => Self::Fixed(*pixels as i32),
            Value::String(text) => {
                let text = text.trim();
                match text.strip_suffix('%') {
                    Some(percent) => Self::Percent(percent.trim().parse().map_err(|_| {
                        anyhow!("Invalid {key} value in {rule_id}: {text:?} is not a percentage")
                    })?),
                    None => Self::Fixed(text.trim_end_matches("px").trim().parse().map_err(
                        |_| {
                            anyhow!(
                                "Invalid {key} value in {rule_id}: expected pixels (\"400\") or a percentage (\"60%\"), got {text:?}"
                            )
                        },
                    )?),
                }
            }
            other => bail!(
                "Invalid {key} value in {rule_id}: \
                 expected pixels (\"400\") or a percentage (\"60%\"), got {other}"
            ),
        };

        match size {
            Self::Fixed(pixels) if pixels <= 0 => {
                bail!("Invalid {key} value in {rule_id}: size must be positive")
            }
            Self::Percent(percent) if !(percent > 0.0 && percent <= 100.0) => bail!(
                "Invalid {key} value in {rule_id}: \
                 percentage must be above 0 and at most 100"
            ),
            _ => Ok(size),
        }
    }
}

/// A window that toggles between parked and shown on top: the "dropdown
/// terminal" pattern, without a shell script.
#[derive(Debug, Clone)]
pub struct Scratchpad {
    pub name: String,
    /// Matching fields, exactly like a rule; `None` means focus-tracking.
    matcher: Option<CompiledRule>,
    /// Command started when no matching window exists.
    pub spawn: Option<Vec<String>>,
    /// Whether showing it should float it.
    pub float: bool,
    pub width: Option<Size>,
    pub height: Option<Size>,
}

impl Scratchpad {
    /// Whether this scratchpad acts on the focused window.
    pub fn follows_focus(&self) -> bool {
        self.matcher.is_none()
    }

    /// Whether a window matches; a focus-tracking scratchpad matches nothing.
    pub fn matches(&self, window: &WindowFacts<'_>) -> bool {
        self.matcher
            .as_ref()
            .is_some_and(|matcher| matcher.matches(window))
    }

    /// The size it asks for, as configured.
    pub fn describe_size(&self) -> String {
        let render = |size: Option<Size>| match size {
            Some(Size::Fixed(pixels)) => format!("{pixels}px"),
            Some(Size::Percent(percent)) => format!("{percent}%"),
            None => "auto".to_string(),
        };
        format!("{}x{}", render(self.width), render(self.height))
    }
}

/// Facts about a window that rules can match on.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowFacts<'a> {
    pub app_id: Option<&'a str>,
    pub title: Option<&'a str>,
    pub floating: bool,
}

/// A rule that matched a window, with what is needed to apply it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedRule {
    pub action: WindowAction,
    /// `sticky.<name>` or `stage.<name>`, as written in the config.
    pub id: String,
    /// Outputs the window is pinned to, in preference order (possibly empty).
    pub outputs: Vec<String>,
}

/// One compiled rule, as reported by `nsticky config check`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RuleSummary {
    /// `sticky.<name>` or `stage.<name>`.
    pub id: String,
    /// Whether the rule is restricted to floating or tiled windows.
    pub floating: Option<bool>,
    /// Outputs the rule pins the window to, in preference order.
    pub outputs: Vec<String>,
    pub app_id: usize,
    pub title: usize,
    pub exclude_app_id: usize,
    pub exclude_title: usize,
}

impl RuleSummary {
    /// Pattern counts, e.g. `app-id: 3, exclude-title: 1`.
    pub fn fields(&self) -> String {
        let mut fields = Vec::new();
        if let Some(floating) = self.floating {
            fields.push(format!("floating: {floating}"));
        }
        for (label, count) in [
            ("app-id", self.app_id),
            ("title", self.title),
            ("exclude-app-id", self.exclude_app_id),
            ("exclude-title", self.exclude_title),
        ] {
            if count > 0 {
                fields.push(format!("{label}: {count}"));
            }
        }
        fields.join(", ")
    }
}

/// A sticky rule compiled to regexes.
///
/// A window matches when every positive field matches (`app_id` AND `title`,
/// each an OR over its patterns) and no exclusion does. Negation lives here
/// because the `regex` crate has no lookaround.
#[derive(Debug, Clone)]
struct CompiledRule {
    /// `sticky.<name>` or `stage.<name>`, as written in the config.
    id: String,
    /// `true`/`false` restricts the rule to floating/tiled windows; `None`
    /// matches either.
    floating: Option<bool>,
    /// Outputs the window is pinned to, in preference order; empty means the
    /// window follows the focused workspace.
    outputs: Vec<String>,
    app_id: Vec<Regex>,
    title: Vec<Regex>,
    exclude_app_id: Vec<Regex>,
    exclude_title: Vec<Regex>,
}

/// No patterns means no constraint; otherwise any pattern may match. A missing
/// value never matches a configured pattern set.
fn matches_any(patterns: &[Regex], value: Option<&str>) -> bool {
    if patterns.is_empty() {
        return true;
    }
    match value {
        Some(value) => patterns.iter().any(|regex| regex.is_match(value)),
        None => false,
    }
}

/// Any exclusion pattern matching the value excludes it; empty sets and a
/// missing value never do.
fn is_excluded(patterns: &[Regex], value: Option<&str>) -> bool {
    match value {
        Some(value) => patterns.iter().any(|regex| regex.is_match(value)),
        None => false,
    }
}

impl CompiledRule {
    fn matches(&self, window: &WindowFacts<'_>) -> bool {
        if let Some(floating) = self.floating
            && floating != window.floating
        {
            return false;
        }

        matches_any(&self.app_id, window.app_id)
            && matches_any(&self.title, window.title)
            && !is_excluded(&self.exclude_app_id, window.app_id)
            && !is_excluded(&self.exclude_title, window.title)
    }

    fn summary(&self) -> RuleSummary {
        RuleSummary {
            id: self.id.clone(),
            floating: self.floating,
            outputs: self.outputs.clone(),
            app_id: self.app_id.len(),
            title: self.title.len(),
            exclude_app_id: self.exclude_app_id.len(),
            exclude_title: self.exclude_title.len(),
        }
    }

    /// `floating` narrows on its own, so a rule with nothing but exclusions has
    /// no constraint left and would match every window.
    fn has_positive_constraint(&self) -> bool {
        self.floating.is_some() || !self.app_id.is_empty() || !self.title.is_empty()
    }
}

/// Compile patterns, naming the rule, the field and the pattern when one fails.
fn compile_patterns(patterns: &[String], field: &str, rule_id: &str) -> Result<Vec<Regex>> {
    patterns
        .iter()
        .map(|pattern| {
            Regex::new(pattern)
                .map_err(|e| anyhow!("Invalid {field} regex in {rule_id}: {pattern:?}\n{e}"))
        })
        .collect()
}

/// Collect a field's patterns from a bare string or an array of strings, with
/// every alias present merged in.
fn collect_patterns(
    rule_table: &toml::Table,
    keys: &[&str],
    field: &str,
    rule_id: &str,
) -> Result<Vec<String>> {
    let mut patterns = Vec::new();
    for key in keys {
        let Some(value) = rule_table.get(*key) else {
            continue;
        };
        match value {
            Value::String(s) => patterns.push(s.clone()),
            Value::Array(items) => {
                for item in items {
                    let Some(s) = item.as_str() else {
                        bail!("Invalid {field} value in {rule_id}: expected a string, got {item}");
                    };
                    patterns.push(s.to_string());
                }
            }
            other => {
                bail!(
                    "Invalid {field} value in {rule_id}: \
                     expected a string or an array of strings, got {other}"
                );
            }
        }
    }
    Ok(patterns)
}

/// Read the optional `output` pins of a rule, in preference order.
fn parse_outputs(rule_table: &toml::Table, rule_id: &str) -> Result<Vec<String>> {
    let mut outputs: Option<Vec<String>> = None;

    for key in OUTPUT_KEYS {
        let Some(value) = rule_table.get(key) else {
            continue;
        };

        let parsed: Vec<String> = match value {
            Value::String(name) => vec![name.clone()],
            Value::Array(items) => items
                .iter()
                .map(|item| {
                    item.as_str().map(String::from).ok_or_else(|| {
                        anyhow!("Invalid {key} value in {rule_id}: expected a string, got {item}")
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            other => bail!(
                "Invalid {key} value in {rule_id}: \
                 expected a string or an array of strings, got {other}"
            ),
        };

        if parsed.is_empty() || parsed.iter().any(|name| name.trim().is_empty()) {
            bail!("Invalid {key} value in {rule_id}: output names must not be empty");
        }

        if let Some(previous) = &outputs
            && previous != &parsed
        {
            bail!("Conflicting output lists in {rule_id}: {previous:?} and {parsed:?}");
        }
        outputs = Some(parsed);
    }

    Ok(outputs.unwrap_or_default())
}

/// Read the optional `floating` constraint of a rule.
fn parse_floating(rule_table: &toml::Table, rule_id: &str) -> Result<Option<bool>> {
    let mut floating = None;

    for key in FLOATING_KEYS {
        let Some(value) = rule_table.get(key) else {
            continue;
        };
        let Some(value) = value.as_bool() else {
            bail!("Invalid {key} value in {rule_id}: expected a boolean, got {value}");
        };
        floating = Some(value);
    }

    Ok(floating)
}

/// Read `stage-keep-workspace`, false by default so an empty stage leaves no
/// trace.
fn parse_stage_keep_workspace(table: &toml::Table) -> Result<bool> {
    let mut keep = false;

    for key in STAGE_KEEP_KEYS {
        let Some(value) = table.get(key) else {
            continue;
        };
        let Some(value) = value.as_bool() else {
            bail!("Invalid {key} value: expected a boolean, got {value}");
        };
        keep = value;
    }

    Ok(keep)
}

/// Read `sticky-follow`, rejecting unknown values instead of guessing.
fn parse_sticky_follow(table: &toml::Table) -> Result<StickyFollow> {
    let mut follow = StickyFollow::default();

    for key in STICKY_FOLLOW_KEYS {
        if let Some(value) = table.get(key) {
            follow = StickyFollow::parse(value, key)?;
        }
    }

    Ok(follow)
}

/// Read a workspace name, rejecting empty or conflicting values.
fn parse_named_workspace(table: &toml::Table, keys: &[&str], default: &str) -> Result<String> {
    parse_workspace_name(table, keys)?.map_or_else(|| Ok(default.to_string()), Ok)
}

fn parse_workspace_name(table: &toml::Table, keys: &[&str]) -> Result<Option<String>> {
    let mut name: Option<String> = None;

    for key in keys {
        let Some(value) = table.get(*key) else {
            continue;
        };
        let Some(workspace) = value.as_str() else {
            bail!("Invalid {key} value: expected a string, got {value}");
        };
        if workspace.trim().is_empty() {
            bail!("Invalid {key} value: workspace name must not be empty");
        }
        if let Some(previous) = &name
            && previous != workspace
        {
            bail!("Conflicting {key} values: {previous:?} and {workspace:?}");
        }
        name = Some(workspace.to_string());
    }

    Ok(name)
}

fn parse_menu(value: &Value) -> Result<MenuSpec> {
    match value {
        Value::String(s) => Ok(MenuSpec::CommandLine(s.clone())),
        Value::Array(items) => {
            let args = items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(String::from)
                        .ok_or_else(|| anyhow!("Invalid menu entry: expected a string, got {item}"))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(MenuSpec::Args(args))
        }
        other => bail!("Invalid menu value: expected a string or an array of strings, got {other}"),
    }
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config: {path:?}"))?;
        Self::parse_document(&content).with_context(|| format!("Failed to parse TOML: {path:?}"))
    }

    /// Parse a TOML document, without touching the filesystem.
    fn parse_document(content: &str) -> Result<Self> {
        // toml 1.x `FromStr for Value` parses a single value; a whole document
        // has to go through `Table`.
        let table: toml::Table = content.parse()?;

        let menu = match table.get("menu") {
            Some(value) => Some(parse_menu(value)?),
            None => None,
        };

        let stage_workspace =
            parse_named_workspace(&table, &STAGE_WORKSPACE_KEYS, DEFAULT_STAGE_WORKSPACE)?;
        let scratchpad_workspace = parse_named_workspace(
            &table,
            &SCRATCHPAD_WORKSPACE_KEYS,
            DEFAULT_SCRATCHPAD_WORKSPACE,
        )?;
        // Two parking areas need two workspaces: sharing one would make each
        // area's windows land on the other's and leave their order undefined.
        if stage_workspace == scratchpad_workspace {
            bail!(
                "stage-workspace and scratchpad-workspace cannot both be {stage_workspace:?}: \
                 each parking area needs a workspace of its own"
            );
        }

        let sticky_follow = parse_sticky_follow(&table)?;
        let stage_keep_workspace = parse_stage_keep_workspace(&table)?;

        Ok(Config {
            sticky_rules: compile_section(&table, "sticky")?,
            stage_rules: compile_section(&table, "stage")?,
            menu,
            stage_workspace,
            scratchpad_workspace,
            stage_keep_workspace,
            sticky_follow,
            scratchpads: compile_scratchpads(&table)?,
        })
    }

    pub fn default_config_dir() -> PathBuf {
        Self::config_dir_under(dirs::config_dir())
    }

    /// Separate from the environment lookup so the fallback is testable.
    fn config_dir_under(base: Option<PathBuf>) -> PathBuf {
        base.unwrap_or_else(|| PathBuf::from("/tmp/nsticky"))
            .join("nsticky")
    }

    pub fn default_config_path() -> PathBuf {
        Self::default_config_dir().join("config.toml")
    }

    pub fn load_or_default() -> Self {
        Self::load_or_default_at(Self::default_config_path())
    }

    /// A missing or broken file must not keep the daemon from starting; the
    /// path is a parameter so both outcomes are testable.
    fn load_or_default_at(path: impl AsRef<Path>) -> Self {
        match Self::load(path) {
            Ok(config) => config,
            Err(e) => {
                // `{e:#}` keeps the parser's line/column details visible.
                tracing::warn!("Failed to load config: {e:#}, using default (no rules)");
                Config::default()
            }
        }
    }

    /// Parse a config from a TOML string (tests only).
    #[cfg(test)]
    pub(crate) fn from_toml(content: &str) -> Result<Self> {
        Self::parse_document(content)
    }

    /// Selector command used by `stage restore`, if configured.
    pub fn menu(&self) -> Option<&MenuSpec> {
        self.menu.as_ref()
    }

    /// Workspace staged windows are parked on (`stage` unless configured).
    pub fn stage_workspace(&self) -> &str {
        &self.stage_workspace
    }

    pub fn scratchpad(&self, name: &str) -> Option<&Scratchpad> {
        self.scratchpads
            .iter()
            .find(|scratchpad| scratchpad.name == name)
    }

    /// Names of the configured scratchpads, for error messages.
    pub fn scratchpad_names(&self) -> Vec<&str> {
        self.scratchpads
            .iter()
            .map(|scratchpad| scratchpad.name.as_str())
            .collect()
    }

    /// Workspace parked scratchpad windows live on.
    pub fn scratchpad_workspace(&self) -> &str {
        &self.scratchpad_workspace
    }

    /// Whether the stage workspace keeps its name (and therefore exists) while
    /// nothing is parked on it.
    pub fn stage_keep_workspace(&self) -> bool {
        self.stage_keep_workspace
    }

    /// How sticky windows without an `output` pin follow workspaces.
    pub fn sticky_follow(&self) -> StickyFollow {
        self.sticky_follow
    }

    /// Every compiled rule, in match precedence order.
    pub fn rules(&self) -> Vec<(WindowAction, RuleSummary)> {
        RULE_SECTIONS
            .iter()
            .flat_map(|&(action, section)| {
                let rules = match section {
                    "sticky" => &self.sticky_rules,
                    _ => &self.stage_rules,
                };
                rules.iter().map(move |rule| (action, rule.summary()))
            })
            .collect()
    }

    /// Sticky rules are checked before stage rules, so adding a stage rule
    /// never changes how an existing sticky configuration behaves.
    pub fn match_rule(&self, window: &WindowFacts<'_>) -> Option<MatchedRule> {
        RULE_SECTIONS.iter().find_map(|&(action, section)| {
            let rules = match section {
                "sticky" => &self.sticky_rules,
                _ => &self.stage_rules,
            };
            rules
                .iter()
                .find(|rule| rule.matches(window))
                .map(|rule| MatchedRule {
                    action,
                    id: rule.id.clone(),
                    outputs: rule.outputs.clone(),
                })
        })
    }
}

/// Compile every `[scratchpad.<name>]` section.
fn compile_scratchpads(table: &toml::Table) -> Result<Vec<Scratchpad>> {
    let Some(value) = table.get("scratchpad") else {
        return Ok(Vec::new());
    };
    let Some(section) = value.as_table() else {
        bail!("Invalid scratchpad section: expected a table, got {value}");
    };

    let mut scratchpads = Vec::new();
    for (name, value) in section {
        let Some(rule_table) = value.as_table() else {
            continue;
        };
        let rule_id = format!("scratchpad.{name}");
        let compiled = compile_rule(rule_table, "scratchpad", name)?;
        // No matching field means the scratchpad acts on the focused window.
        let matcher = compiled.has_positive_constraint().then_some(compiled);

        scratchpads.push(Scratchpad {
            name: name.clone(),
            matcher,
            spawn: parse_spawn(rule_table, &rule_id)?,
            float: parse_bool(rule_table, &FLOAT_KEYS, "float", &rule_id)?.unwrap_or(true),
            width: parse_size(rule_table, &WIDTH_KEYS, "width", &rule_id)?,
            height: parse_size(rule_table, &HEIGHT_KEYS, "height", &rule_id)?,
        });
    }
    Ok(scratchpads)
}

fn parse_spawn(rule_table: &toml::Table, rule_id: &str) -> Result<Option<Vec<String>>> {
    let mut spawn = None;

    for key in SPAWN_KEYS {
        let Some(value) = rule_table.get(key) else {
            continue;
        };
        let Value::Array(items) = value else {
            bail!(
                "Invalid {key} value in {rule_id}: \
                 expected an array of strings, got {value}"
            );
        };

        let argv: Vec<String> = items
            .iter()
            .map(|item| {
                item.as_str().map(String::from).ok_or_else(|| {
                    anyhow!("Invalid {key} value in {rule_id}: expected a string, got {item}")
                })
            })
            .collect::<Result<Vec<_>>>()?;

        if argv.is_empty() || argv[0].trim().is_empty() {
            bail!("Invalid {key} value in {rule_id}: the command must not be empty");
        }
        spawn = Some(argv);
    }

    Ok(spawn)
}

fn parse_bool(
    rule_table: &toml::Table,
    keys: &[&str],
    field: &str,
    rule_id: &str,
) -> Result<Option<bool>> {
    let mut parsed = None;
    for key in keys {
        let Some(value) = rule_table.get(*key) else {
            continue;
        };
        let Some(value) = value.as_bool() else {
            bail!("Invalid {key} value in {rule_id}: expected a boolean, got {value}");
        };
        parsed = Some(value);
    }
    let _ = field;
    Ok(parsed)
}

fn parse_size(
    rule_table: &toml::Table,
    keys: &[&str],
    field: &str,
    rule_id: &str,
) -> Result<Option<Size>> {
    let mut parsed = None;
    for key in keys {
        if let Some(value) = rule_table.get(*key) {
            parsed = Some(Size::parse(value, key, rule_id)?);
        }
    }
    let _ = field;
    Ok(parsed)
}

/// Compile every rule of a `[<section>.<name>]` table.
fn compile_section(table: &toml::Table, section: &str) -> Result<Vec<CompiledRule>> {
    let Some(value) = table.get(section) else {
        return Ok(Vec::new());
    };
    let Some(section_table) = value.as_table() else {
        bail!("Invalid {section} section: expected a table, got {value}");
    };

    let mut rules = Vec::new();
    for (name, value) in section_table {
        let Some(rule_table) = value.as_table() else {
            continue;
        };
        let rule = compile_rule(rule_table, section, name)?;
        // Rules without a constraint would match every window.
        if rule.has_positive_constraint() {
            rules.push(rule);
        }
    }
    Ok(rules)
}

fn compile_rule(rule_table: &toml::Table, section: &str, name: &str) -> Result<CompiledRule> {
    // Errors name the rule as the user wrote it: `sticky.discord`, `stage.games`.
    let rule_id = format!("{section}.{name}");
    Ok(CompiledRule {
        id: rule_id.clone(),
        floating: parse_floating(rule_table, &rule_id)?,
        outputs: parse_outputs(rule_table, &rule_id)?,
        app_id: compile_patterns(
            &collect_patterns(rule_table, &APP_ID_KEYS, "app-id", &rule_id)?,
            "app-id",
            &rule_id,
        )?,
        title: compile_patterns(
            &collect_patterns(rule_table, &TITLE_KEYS, "title", &rule_id)?,
            "title",
            &rule_id,
        )?,
        exclude_app_id: compile_patterns(
            &collect_patterns(rule_table, &EXCLUDE_APP_ID_KEYS, "exclude-app-id", &rule_id)?,
            "exclude-app-id",
            &rule_id,
        )?,
        exclude_title: compile_patterns(
            &collect_patterns(rule_table, &EXCLUDE_TITLE_KEYS, "exclude-title", &rule_id)?,
            "exclude-title",
            &rule_id,
        )?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Window facts for a tiled window with this app id and title.
    fn facts<'a>(app_id: Option<&'a str>, title: Option<&'a str>) -> WindowFacts<'a> {
        WindowFacts {
            app_id,
            title,
            floating: false,
        }
    }

    /// Sticky-match predicate for the rule tests.
    fn matched(config: &Config, app_id: &Option<String>, title: &Option<String>) -> bool {
        config
            .match_rule(&facts(app_id.as_deref(), title.as_deref()))
            .map(|matched| matched.action)
            == Some(WindowAction::Sticky)
    }

    fn config_from_toml(content: &str) -> Config {
        Config::from_toml(content).unwrap_or_else(|e| panic!("config failed to load: {e:#}"))
    }

    fn app(id: &str) -> Option<String> {
        Some(id.to_string())
    }

    fn title(t: &str) -> Option<String> {
        Some(t.to_string())
    }

    const DISCORD_CONFIG: &str = r#"
[sticky.discord]
app-id = [
    '^[Dd]iscord$',
    '^[Vv]encord$',
    '^[Vv]esktop$'
]

exclude-title = [
    '^\(\d+\) [Dd]iscord \|.*$',
    '^[Dd]iscord$|^[Vv]esktop$|^[Vv]encord$',
    '^[Cc]anal$|^[Cc]hannel$|^[Dd]irectos$|^[Ss]treams?$'
]
"#;

    #[test]
    fn test_match_app_id_only() {
        let config = config_from_toml(
            r#"
[sticky.firefox]
app_id = "firefox"
"#,
        );
        assert!(matched(&config, &Some("firefox".to_string()), &None));
        assert!(!matched(&config, &Some("chrome".to_string()), &None));
    }

    #[test]
    fn test_match_app_id_string_and_array() {
        let config = config_from_toml(
            r#"
[sticky.firefox]
app-id = "firefox"

[sticky.players]
app-id = ["Spotify", "Amberol"]
"#,
        );
        assert!(matched(&config, &app("firefox"), &None));
        assert!(matched(&config, &app("Spotify"), &None));
        assert!(matched(&config, &app("Amberol"), &None));
        assert!(!matched(&config, &app("mpv"), &None));
    }

    #[test]
    fn test_match_title_only() {
        let config = config_from_toml(
            r#"
[sticky.gmail]
title = "Gmail"
"#,
        );
        assert!(matched(&config, &None, &Some("Inbox - Gmail".to_string())));
        assert!(!matched(&config, &None, &Some("YouTube".to_string())));
        // A rule with a title but no app-id matches any application.
        assert!(matched(&config, &app("firefox"), &title("Gmail")));
        // A missing title cannot match a configured title pattern.
        assert!(!matched(&config, &app("firefox"), &None));
    }

    #[test]
    fn test_match_title_array() {
        let config = config_from_toml(
            r#"
[sticky.docs]
title = ["^foo$", "^bar$"]
"#,
        );
        assert!(matched(&config, &app("any"), &title("foo")));
        assert!(matched(&config, &app("any"), &title("bar")));
        assert!(!matched(&config, &app("any"), &title("baz")));
    }

    #[test]
    fn test_match_both_and() {
        let config = config_from_toml(
            r#"
[sticky.firefox-gmail]
app_id = "firefox"
title = "Gmail"
"#,
        );
        assert!(matched(
            &config,
            &Some("firefox".to_string()),
            &Some("Inbox - Gmail".to_string())
        ));
        assert!(!matched(
            &config,
            &Some("firefox".to_string()),
            &Some("YouTube".to_string())
        ));
        assert!(!matched(
            &config,
            &Some("chrome".to_string()),
            &Some("Gmail".to_string())
        ));
    }

    #[test]
    fn test_multiple_rules() {
        let config = config_from_toml(
            r#"
[sticky.firefox]
app_id = "firefox"

[sticky.chromium]
app_id = "chromium"
"#,
        );
        assert!(matched(&config, &Some("firefox".to_string()), &None));
        assert!(matched(&config, &Some("chromium".to_string()), &None));
        assert!(!matched(&config, &Some("chrome".to_string()), &None));
    }

    #[test]
    fn test_no_rules() {
        let config = config_from_toml("");
        assert!(!matched(
            &config,
            &Some("firefox".to_string()),
            &Some("test".to_string())
        ));
    }

    #[test]
    fn test_missing_file_returns_err() {
        let result = Config::load("/tmp/nsticky_nonexistent_test_file_12345.toml");
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_file_returns_default() {
        let config = config_from_toml("");
        assert!(!matched(
            &config,
            &Some("firefox".to_string()),
            &Some("test".to_string())
        ));
    }

    #[test]
    fn test_unknown_fields_are_ignored_gracefully() {
        let config = config_from_toml(
            r#"
[sticky.firefox]
app_id = "firefox"
unknown_field = "should_be_ignored"

[unrelated_table]
some_key = "also_ignored"
"#,
        );
        assert!(matched(&config, &Some("firefox".to_string()), &None));
        assert!(!matched(&config, &Some("chrome".to_string()), &None));
    }

    #[test]
    fn test_invalid_regex() {
        let result = Config::from_toml(
            r#"
[sticky.test]
app_id = "[invalid"
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_rule_without_trailing_title_matches_any_title() {
        let config = config_from_toml(
            r#"
[sticky.pavucontrol]
app-id = "pavucontrol"
"#,
        );
        assert!(matched(
            &config,
            &app("pavucontrol"),
            &title("Volume Control")
        ));
        assert!(matched(&config, &app("pavucontrol"), &None));
        assert!(!matched(&config, &app("firefox"), &title("Volume Control")));
    }

    #[test]
    fn test_exclude_title() {
        let config = config_from_toml(
            r#"
[sticky.firefox]
app-id = "firefox"
exclude-title = "Gmail"
"#,
        );
        assert!(matched(&config, &app("firefox"), &title("YouTube")));
        assert!(!matched(&config, &app("firefox"), &title("Inbox - Gmail")));
    }

    #[test]
    fn test_multiple_exclude_title_patterns() {
        let config = config_from_toml(
            r#"
[sticky.firefox]
app-id = "firefox"
exclude-title = ["^foo$", "^bar$"]
"#,
        );
        assert!(matched(&config, &app("firefox"), &title("baz")));
        // Both exclusions apply: NOT foo AND NOT bar.
        assert!(!matched(&config, &app("firefox"), &title("foo")));
        assert!(!matched(&config, &app("firefox"), &title("bar")));
    }

    #[test]
    fn test_exclude_app_id() {
        let config = config_from_toml(
            r#"
[sticky.terminals]
app-id = ["kitty", "foot"]
exclude-app-id = "kitty"
"#,
        );
        assert!(matched(&config, &app("foot"), &None));
        assert!(!matched(&config, &app("kitty"), &None));
    }

    #[test]
    fn test_multiple_exclude_app_id_patterns() {
        let config = config_from_toml(
            r#"
[sticky.terminals]
app-id = ["kitty", "foot", "alacritty"]
exclude-app-id = ["kitty", "foot"]
"#,
        );
        assert!(matched(&config, &app("alacritty"), &None));
        assert!(!matched(&config, &app("kitty"), &None));
        assert!(!matched(&config, &app("foot"), &None));
    }

    #[test]
    fn test_exclusion_does_not_match_missing_attribute() {
        let config = config_from_toml(
            r#"
[sticky.window]
app-id = ".*"
exclude-app-id = "firefox"
exclude-title = "Gmail"
"#,
        );
        // A missing title must not trigger the title exclusion.
        assert!(matched(&config, &app("chromium"), &None));
        assert!(!matched(&config, &app("firefox"), &None));
        assert!(!matched(&config, &app("chromium"), &title("Gmail")));
        // A missing app-id cannot satisfy the positive app-id constraint either.
        assert!(!matched(&config, &None, &title("Home")));
    }

    #[test]
    fn test_positive_and_negative_combined() {
        let config = config_from_toml(
            r#"
[sticky.combo]
app-id = ["^firefox$", "^chromium$"]
title = ["Gmail", "Docs"]
exclude-app-id = "^chromium$"
exclude-title = ["^Spam"]
"#,
        );
        assert!(matched(&config, &app("firefox"), &title("Inbox - Gmail")));
        assert!(!matched(&config, &app("chromium"), &title("Gmail")));
        assert!(!matched(&config, &app("firefox"), &title("Spam folder")));
        assert!(!matched(&config, &app("chrome"), &title("Gmail")));
    }

    #[test]
    fn test_case_insensitive_regex() {
        let config = config_from_toml(
            r#"
[sticky.discord]
title = '(?i)^discord$'
"#,
        );
        for value in ["Discord", "DISCORD", "discord", "DiScOrD"] {
            assert!(matched(&config, &None, &title(value)), "{value}");
        }
    }

    #[test]
    fn test_missing_app_id_against_rule_with_app_id() {
        let config = config_from_toml(
            r#"
[sticky.firefox]
app-id = "firefox"
"#,
        );
        assert!(!matched(&config, &None, &title("anything")));
    }

    #[test]
    fn test_underscore_aliases() {
        let config = config_from_toml(
            r#"
[sticky.legacy]
app_id = "firefox"
exclude_title = "Gmail"
exclude_app_id = "firefox-esr"
"#,
        );
        assert!(matched(&config, &app("firefox"), &title("YouTube")));
        assert!(!matched(&config, &app("firefox"), &title("Gmail")));
        assert!(!matched(&config, &app("firefox-esr"), &title("YouTube")));
    }

    #[test]
    fn test_not_aliases_are_accepted() {
        let config = config_from_toml(
            r#"
[sticky.firefox]
app-id = "firefox"
not-title = "Gmail"
not-app-id = "firefox-esr"
"#,
        );
        assert!(matched(&config, &app("firefox"), &title("YouTube")));
        assert!(!matched(&config, &app("firefox"), &title("Gmail")));
        assert!(!matched(&config, &app("firefox-esr"), &title("YouTube")));
    }

    #[test]
    fn test_aliases_are_combined_not_overwritten() {
        let config = config_from_toml(
            r#"
[sticky.mixed]
app-id = "firefox"
app_id = "chromium"
exclude-title = ["Gmail"]
exclude_title = ["Docs"]
not-title = "YouTube"
"#,
        );
        assert!(matched(&config, &app("firefox"), &title("Home")));
        assert!(matched(&config, &app("chromium"), &title("Home")));
        assert!(!matched(&config, &app("firefox"), &title("Gmail")));
        assert!(!matched(&config, &app("firefox"), &title("Docs")));
        assert!(!matched(&config, &app("firefox"), &title("YouTube")));
    }

    #[test]
    fn test_hyphen_and_underscore_names_are_equivalent() {
        let hyphenated = r#"
[sticky.x]
app-id = ["kitty"]
exclude-title = ["^Gmail$"]
exclude-app-id = ["^foot$"]
"#;
        let underscored = r#"
[sticky.x]
app_id = ["kitty"]
exclude_title = ["^Gmail$"]
exclude_app_id = ["^foot$"]
"#;
        let a = config_from_toml(hyphenated);
        let b = config_from_toml(underscored);
        for (id, t) in [
            (Some("kitty"), Some("Terminal")),
            (Some("foot"), Some("Terminal")),
            (Some("kitty"), Some("Gmail")),
            (Some("x"), None),
        ] {
            assert_eq!(
                matched(&a, &id.map(String::from), &t.map(String::from)),
                matched(&b, &id.map(String::from), &t.map(String::from)),
            );
        }
    }

    #[test]
    fn test_discord_rule_matrix() {
        let config = config_from_toml(DISCORD_CONFIG);

        let sticky = [
            ("Discord", "General"),
            ("Discord", "Gaming"),
            ("Vencord", "General"),
            ("Vesktop", "Gaming"),
        ];
        for (id, t) in sticky {
            assert!(matched(&config, &app(id), &title(t)), "{id} / {t}");
        }

        let not_sticky = [
            ("Discord", "(3) Discord | General"),
            ("Discord", "Discord"),
            ("Discord", "Vesktop"),
            ("Discord", "Vencord"),
            ("Discord", "Canal"),
            ("Discord", "Channel"),
            ("Discord", "Directos"),
            ("Discord", "Stream"),
            ("Discord", "Streams"),
            ("Firefox", "General"),
        ];
        for (id, t) in not_sticky {
            assert!(!matched(&config, &app(id), &title(t)), "{id} / {t}");
        }
    }

    #[test]
    fn test_invalid_exclude_regex_reports_rule_field_and_pattern() {
        let result = Config::from_toml(
            r#"
[sticky.discord]
app-id = "discord"
exclude-title = ["^(Discord$"]
"#,
        );
        let err = result.map(|_| ()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Invalid exclude-title regex in sticky.discord: \"^(Discord$\""),
            "unexpected error: {msg}"
        );
        assert!(msg.contains("regex parse error"), "unexpected error: {msg}");
    }

    #[test]
    fn test_invalid_title_regex_reports_field() {
        let result = Config::from_toml(
            r#"
[sticky.discord]
title = "["
"#,
        );
        let msg = format!("{:#}", result.map(|_| ()).unwrap_err());
        assert!(
            msg.contains("Invalid title regex in sticky.discord: \"[\""),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn test_wrong_value_type_is_reported() {
        for value in ["42", "true", "{ name = \"x\" }"] {
            let toml = format!("[sticky.discord]\napp-id = {value}\n");
            let msg = format!("{:#}", Config::from_toml(&toml).unwrap_err());
            assert!(
                msg.contains("Invalid app-id value in sticky.discord"),
                "{value}: unexpected error: {msg}"
            );
            assert!(
                msg.contains("expected a string or an array of strings"),
                "{value}: unexpected error: {msg}"
            );
        }
    }

    #[test]
    fn test_array_with_non_string_entry_is_reported() {
        let result = Config::from_toml(
            r#"
[sticky.discord]
title = ["ok", 3]
"#,
        );
        let msg = format!("{:#}", result.map(|_| ()).unwrap_err());
        assert!(
            msg.contains("Invalid title value in sticky.discord"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn test_rule_without_positive_constraint_is_ignored() {
        let config = config_from_toml(
            r#"
[sticky.only-exclusions]
exclude-title = "Gmail"

[sticky.firefox]
app-id = "firefox"
"#,
        );
        // An exclusion-only rule matches nothing...
        assert!(!matched(&config, &app("chrome"), &title("Home")));
        // ...and never vetoes the rules that do match.
        assert!(matched(&config, &app("firefox"), &title("Gmail")));
    }

    #[test]
    fn test_menu_string_is_parsed_with_quoting() {
        let config = config_from_toml(
            r#"
menu = "vicinae dmenu --placeholder 'Restore Window:'"
"#,
        );
        let menu = config.menu().expect("menu configured").parse().unwrap();
        assert_eq!(menu.program(), "vicinae");
        assert_eq!(menu.args(), ["dmenu", "--placeholder", "Restore Window:"]);
    }

    #[test]
    fn test_menu_array_needs_no_shell_parsing() {
        let config = config_from_toml(
            r#"
menu = ["rofi", "-dmenu", "-p", "Restore Window:"]
"#,
        );
        let menu = config.menu().expect("menu configured").parse().unwrap();
        assert_eq!(menu.program(), "rofi");
        assert_eq!(menu.args(), ["-dmenu", "-p", "Restore Window:"]);
    }

    #[test]
    fn test_menu_without_quoting_still_works() {
        for (value, program, args) in [
            ("rofi -dmenu", "rofi", vec!["-dmenu"]),
            ("fuzzel --dmenu", "fuzzel", vec!["--dmenu"]),
            ("wofi --show dmenu", "wofi", vec!["--show", "dmenu"]),
        ] {
            let config = config_from_toml(&format!("menu = \"{value}\"\n"));
            let menu = config.menu().expect("menu configured").parse().unwrap();
            assert_eq!(menu.program(), program);
            assert_eq!(menu.args(), args);
        }
    }

    #[test]
    fn test_invalid_menu_value_type_is_reported() {
        let result = Config::from_toml("menu = 3\n");
        let msg = format!("{:#}", result.map(|_| ()).unwrap_err());
        assert!(
            msg.contains("Invalid menu value: expected a string or an array of strings"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn test_stage_workspace_defaults_to_stage() {
        assert_eq!(Config::default().stage_workspace(), "stage");
        assert_eq!(config_from_toml("").stage_workspace(), "stage");
    }

    #[test]
    fn test_stage_workspace_is_configurable() {
        let config = config_from_toml("stage-workspace = \"parking\"\n");
        assert_eq!(config.stage_workspace(), "parking");
    }

    #[test]
    fn test_stage_workspace_accepts_the_underscore_spelling() {
        let config = config_from_toml("stage_workspace = \"parking\"\n");
        assert_eq!(config.stage_workspace(), "parking");
    }

    #[test]
    fn test_stage_workspace_is_released_by_default_and_can_be_kept() {
        assert!(
            !Config::default().stage_keep_workspace(),
            "the point of the stage is that it leaves no trace while empty"
        );
        assert!(config_from_toml("stage-keep-workspace = true\n").stage_keep_workspace());
        assert!(config_from_toml("stage_keep_workspace = true\n").stage_keep_workspace());
        assert!(!config_from_toml("stage-keep-workspace = false\n").stage_keep_workspace());
    }

    #[test]
    fn test_invalid_stage_keep_workspace_is_reported() {
        let error = Config::from_toml("stage-keep-workspace = \"yes\"\n").unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("Invalid stage-keep-workspace value: expected a boolean, got \"yes\""),
            "{message}"
        );
    }

    #[test]
    fn test_stage_workspace_rejects_empty_names() {
        let error = Config::from_toml("stage-workspace = \"  \"\n").unwrap_err();
        assert!(
            format!("{error:#}").contains("must not be empty"),
            "{error:#}"
        );
    }

    #[test]
    fn test_the_two_parking_areas_need_different_workspaces() {
        let toml = "stage-workspace = \"parking\"\nscratchpad-workspace = \"parking\"\n";
        let error = Config::from_toml(toml).unwrap_err();
        assert!(
            format!("{error:#}").contains("each parking area needs a workspace of its own"),
            "{error:#}"
        );

        let config =
            Config::from_toml("stage-workspace = \"s\"\nscratchpad-workspace = \"p\"\n").unwrap();
        assert_eq!(config.stage_workspace(), "s");
        assert_eq!(config.scratchpad_workspace(), "p");
    }

    #[test]
    fn test_workspace_keys_reject_conflicting_aliases() {
        for toml in [
            "stage-workspace = \"a\"\nstage_workspace = \"b\"\n",
            "scratchpad-workspace = \"a\"\nscratchpad_workspace = \"b\"\n",
        ] {
            let error = Config::from_toml(toml).unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains("Conflicting")
                    && message.contains("\"a\"")
                    && message.contains("\"b\""),
                "{message}"
            );
        }
    }

    #[test]
    fn test_stage_workspace_rejects_non_strings() {
        for value in ["3", "true", "{ name = \"x\" }"] {
            let toml = format!("stage-workspace = {value}\n");
            let message = format!("{:#}", Config::from_toml(&toml).unwrap_err());
            assert!(
                message.contains("Invalid stage-workspace value")
                    && message.contains("expected a string"),
                "{value}: {message}"
            );
        }
    }

    #[test]
    fn test_stage_rule_reports_the_stage_action() {
        let config = config_from_toml(
            r#"
[stage.games]
app-id = "steam"
"#,
        );

        assert_eq!(
            config
                .match_rule(&facts(Some("steam"), Some("Steam")))
                .map(|matched| matched.action),
            Some(WindowAction::Stage)
        );
        assert_eq!(
            config
                .match_rule(&facts(Some("firefox"), None))
                .map(|matched| matched.action),
            None
        );
        // A stage rule is not a sticky rule.
        assert!(!matched(&config, &app("steam"), &title("Steam")));
    }

    #[test]
    fn test_sticky_rules_take_precedence_over_stage_rules() {
        let config = config_from_toml(
            r#"
[sticky.browser]
app-id = "firefox"

[stage.overlap]
app-id = "firefox"
"#,
        );

        assert_eq!(
            config
                .match_rule(&facts(Some("firefox"), None))
                .map(|matched| matched.action),
            Some(WindowAction::Sticky)
        );
    }

    #[test]
    fn test_stage_rules_support_arrays_and_exclusions() {
        let config = config_from_toml(
            r#"
[stage.games]
app-id = ["steam", "lutris"]
exclude-title = "^Launcher$"
"#,
        );

        assert_eq!(
            config
                .match_rule(&facts(Some("lutris"), Some("Game")))
                .map(|matched| matched.action),
            Some(WindowAction::Stage)
        );
        assert_eq!(
            config
                .match_rule(&facts(Some("lutris"), Some("Launcher")))
                .map(|matched| matched.action),
            None
        );
        assert_eq!(
            config
                .match_rule(&facts(Some("firefox"), Some("Game")))
                .map(|matched| matched.action),
            None
        );
    }

    #[test]
    fn test_stage_rule_without_positive_constraint_is_ignored() {
        let config = config_from_toml(
            r#"
[stage.only-exclusions]
exclude-title = "Gmail"
"#,
        );

        assert_eq!(
            config
                .match_rule(&facts(Some("anything"), Some("Home")))
                .map(|matched| matched.action),
            None
        );
    }

    #[test]
    fn test_rule_output_pins_are_read_in_order() {
        let config = config_from_toml(
            r#"
[sticky.discord]
app-id = "discord"
output = ["DP-2", "DP-1"]
"#,
        );

        let matched = config
            .match_rule(&facts(Some("discord"), None))
            .expect("rule matches");
        assert_eq!(matched.outputs, vec!["DP-2", "DP-1"]);
        assert_eq!(config.rules()[0].1.outputs, vec!["DP-2", "DP-1"]);
    }

    #[test]
    fn test_rule_output_accepts_a_single_name_and_the_plural_key() {
        let single = config_from_toml(
            r#"
[sticky.discord]
app-id = "discord"
output = "DP-1"
"#,
        );
        assert_eq!(
            single
                .match_rule(&facts(Some("discord"), None))
                .unwrap()
                .outputs,
            vec!["DP-1"]
        );

        let plural = config_from_toml(
            r#"
[sticky.discord]
app-id = "discord"
outputs = ["DP-1"]
"#,
        );
        assert_eq!(
            plural
                .match_rule(&facts(Some("discord"), None))
                .unwrap()
                .outputs,
            vec!["DP-1"]
        );
    }

    #[test]
    fn test_conflicting_output_keys_are_reported() {
        let error = Config::from_toml(
            r#"
[sticky.discord]
app-id = "discord"
output = "DP-1"
outputs = ["DP-2"]
"#,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("Conflicting output lists in sticky.discord"),
            "{error:#}"
        );
    }

    #[test]
    fn test_empty_output_names_are_reported() {
        let error = Config::from_toml(
            r#"
[sticky.discord]
app-id = "discord"
output = "  "
"#,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("output names must not be empty"),
            "{error:#}"
        );
    }

    #[test]
    fn test_sticky_follow_defaults_to_focused_and_is_configurable() {
        assert_eq!(
            Config::default().sticky_follow(),
            StickyFollow::Focused,
            "upstream behaviour stays the default"
        );
        assert_eq!(
            config_from_toml("sticky-follow = \"own-output\"\n").sticky_follow(),
            StickyFollow::OwnOutput
        );
        assert_eq!(
            config_from_toml("sticky_follow = \"focused\"\n").sticky_follow(),
            StickyFollow::Focused
        );
    }

    #[test]
    fn test_invalid_sticky_follow_is_reported() {
        let error = Config::from_toml("sticky-follow = \"sideways\"\n").unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("expected \"focused\" or \"own-output\", got \"sideways\""),
            "{message}"
        );
    }

    #[test]
    fn test_invalid_rule_section_is_reported() {
        let error = Config::from_toml("stage = 3\n").unwrap_err();
        assert!(
            format!("{error:#}").contains("Invalid stage section: expected a table, got 3"),
            "{error:#}"
        );
    }

    #[test]
    fn test_invalid_stage_regex_names_the_section() {
        let error = Config::from_toml(
            r#"
[stage.games]
app-id = "["
"#,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("Invalid app-id regex in stage.games: \"["),
            "{message}"
        );
    }

    #[test]
    fn test_scratchpad_sizes_are_pixels_or_percentages() {
        let config = config_from_toml(
            r#"
[scratchpad.term]
app-id = "foot"
width = "60%"
height = 480
"#,
        );
        let scratchpad = config.scratchpad("term").expect("configured");
        assert_eq!(scratchpad.width, Some(Size::Percent(60.0)));
        assert_eq!(scratchpad.height, Some(Size::Fixed(480)));
        assert_eq!(scratchpad.describe_size(), "60%x480px");
        assert!(scratchpad.float, "floating by default");
    }

    #[test]
    fn test_scratchpad_size_accepts_px_and_rejects_fractions() {
        let pixels = config_from_toml(
            r#"
[scratchpad.term]
app-id = "foot"
width = "800px"
"#,
        );
        assert_eq!(
            pixels.scratchpad("term").unwrap().width,
            Some(Size::Fixed(800))
        );

        // `0.6` meaning 60% is the mistake this guards against.
        let error = Config::from_toml(
            r#"
[scratchpad.term]
app-id = "foot"
width = 0.6
"#,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("expected pixels (\"400\") or a percentage (\"60%\")"),
            "{message}"
        );
    }

    #[test]
    fn test_scratchpad_rejects_impossible_sizes() {
        for (value, expected) in [
            ("\"120%\"", "percentage must be above 0 and at most 100"),
            ("\"0%\"", "percentage must be above 0 and at most 100"),
            ("0", "size must be positive"),
            ("\"wide\"", "expected pixels"),
        ] {
            let toml = format!("[scratchpad.term]\napp-id = \"foot\"\nwidth = {value}\n");
            let error = Config::from_toml(&toml).unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains(expected), "{value}: {message}");
        }
    }

    #[test]
    fn test_scratchpad_without_a_matcher_follows_focus() {
        let config = config_from_toml(
            r#"
[scratchpad.any]
float = true
"#,
        );
        let scratchpad = config.scratchpad("any").expect("configured");

        assert!(scratchpad.follows_focus(), "no fields: the focused window");
        assert!(scratchpad.spawn.is_none());

        let matched = config_from_toml(
            r#"
[scratchpad.term]
app-id = "foot"
"#,
        );
        assert!(!matched.scratchpad("term").unwrap().follows_focus());
    }

    #[test]
    fn test_invalid_scratchpad_section_is_reported() {
        let error = Config::from_toml("scratchpad = 3\n").unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("Invalid scratchpad section: expected a table"),
            "{message}"
        );
    }

    #[test]
    fn test_scratchpad_spawn_command_is_validated() {
        let error = Config::from_toml(
            r#"
[scratchpad.term]
app-id = "foot"
spawn = "foot"
"#,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("expected an array of strings"),
            "{message}"
        );

        let error = Config::from_toml(
            r#"
[scratchpad.term]
app-id = "foot"
spawn = []
"#,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("the command must not be empty"),
            "{error:#}"
        );
    }

    #[test]
    fn test_rules_are_reported_in_precedence_order() {
        let config = config_from_toml(
            r#"
[sticky.discord]
app-id = ["a", "b"]
exclude-title = "c"

[stage.games]
app-id = "steam"
title = "Game"
"#,
        );

        let rules = config.rules();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].0, WindowAction::Sticky);
        assert_eq!(rules[0].1.id, "sticky.discord");
        assert_eq!(rules[0].1.fields(), "app-id: 2, exclude-title: 1");
        assert_eq!(rules[1].0, WindowAction::Stage);
        assert_eq!(rules[1].1.id, "stage.games");
        assert_eq!(rules[1].1.fields(), "app-id: 1, title: 1");
    }

    #[test]
    fn test_floating_rule_matches_only_floating_windows() {
        let config = config_from_toml(
            r#"
[sticky.pip]
floating = true

[sticky.tiled]
floating = false
app-id = "firefox"
"#,
        );

        let floating = WindowFacts {
            app_id: Some("mpv"),
            title: Some("video"),
            floating: true,
        };
        let tiled = WindowFacts {
            app_id: Some("mpv"),
            title: Some("video"),
            floating: false,
        };

        assert_eq!(
            config.match_rule(&floating).map(|m| m.id),
            Some("sticky.pip".to_string())
        );
        assert_eq!(config.match_rule(&tiled), None, "pip requires floating");

        let tiled_browser = WindowFacts {
            app_id: Some("firefox"),
            ..tiled
        };
        assert_eq!(
            config.match_rule(&tiled_browser).map(|m| m.id),
            Some("sticky.tiled".to_string())
        );
        let floating_browser = WindowFacts {
            app_id: Some("firefox"),
            ..floating
        };
        // The tiled rule must not claim a floating window.
        assert_eq!(
            config.match_rule(&floating_browser).map(|m| m.id),
            Some("sticky.pip".to_string())
        );
    }

    #[test]
    fn test_invalid_floating_value_is_reported() {
        let error = Config::from_toml(
            r#"
[sticky.pip]
app-id = "mpv"
floating = "yes"
"#,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message
                .contains("Invalid floating value in sticky.pip: expected a boolean, got \"yes\""),
            "{message}"
        );
    }

    #[test]
    fn test_rule_summary_reports_the_floating_constraint() {
        let config = config_from_toml(
            r#"
[sticky.pip]
app-id = "mpv"
floating = true
"#,
        );
        assert_eq!(config.rules()[0].1.fields(), "floating: true, app-id: 1");
    }

    #[test]
    fn test_rule_summary_omits_unused_fields() {
        let config = config_from_toml(
            r#"
[sticky.simple]
app-id = "firefox"
"#,
        );
        assert_eq!(config.rules()[0].1.fields(), "app-id: 1");
    }

    #[test]
    fn test_no_menu_configured() {
        let config = config_from_toml(
            r#"
[sticky.firefox]
app-id = "firefox"
"#,
        );
        assert!(config.menu().is_none());
    }

    #[test]
    fn test_sticky_follow_names_are_the_documented_ones() {
        assert_eq!(StickyFollow::Focused.as_str(), "focused");
        assert_eq!(StickyFollow::OwnOutput.as_str(), "own-output");

        // What `config check` prints is what the file accepts.
        for name in ["focused", "own-output"] {
            let config = config_from_toml(&format!("sticky-follow = \"{name}\"\n"));
            assert_eq!(config.sticky_follow().as_str(), name);
        }
    }

    #[test]
    fn test_non_string_sticky_follow_is_reported() {
        let error = Config::from_toml("sticky-follow = 3\n").unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("Invalid sticky-follow value")
                && message.contains("expected a string"),
            "{message}"
        );
    }

    #[test]
    fn test_scratchpad_size_rejects_a_non_numeric_percentage() {
        let error = Config::from_toml(
            r#"
[scratchpad.term]
app-id = "foot"
width = "wide%"
"#,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("Invalid width value in scratchpad.term")
                && message.contains("\"wide%\""),
            "{message}"
        );
    }

    #[test]
    fn test_describe_size_reports_auto_when_unset() {
        let config = config_from_toml(
            r#"
[scratchpad.term]
app-id = "foot"
"#,
        );
        let scratchpad = config.scratchpad("term").expect("configured");
        assert_eq!((scratchpad.width, scratchpad.height), (None, None));
        assert_eq!(scratchpad.describe_size(), "autoxauto");
    }

    #[test]
    fn test_output_array_with_a_non_string_entry_is_reported() {
        let error = Config::from_toml(
            r#"
[sticky.discord]
app-id = "discord"
output = ["DP-1", 3]
"#,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("Invalid output value in sticky.discord")
                && message.contains("expected a string, got 3"),
            "{message}"
        );
    }

    #[test]
    fn test_output_as_a_table_is_reported() {
        let error = Config::from_toml(
            r#"
[sticky.discord]
app-id = "discord"
output = { name = "DP-1" }
"#,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("Invalid output value in sticky.discord")
                && message.contains("expected a string or an array of strings"),
            "{message}"
        );
    }

    #[test]
    fn test_spawn_array_with_a_non_string_entry_is_reported() {
        let error = Config::from_toml(
            r#"
[scratchpad.term]
app-id = "foot"
spawn = ["foot", 3]
"#,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("Invalid spawn value in scratchpad.term")
                && message.contains("expected a string, got 3"),
            "{message}"
        );
    }

    #[test]
    fn test_scratchpad_float_must_be_a_boolean() {
        for value in ["\"yes\"", "1"] {
            let toml = format!("[scratchpad.term]\napp-id = \"foot\"\nfloat = {value}\n");
            let message = format!("{:#}", Config::from_toml(&toml).unwrap_err());
            assert!(
                message.contains("Invalid float value in scratchpad.term")
                    && message.contains("expected a boolean"),
                "{value}: {message}"
            );
        }
    }

    #[test]
    fn test_invalid_exclude_app_id_regex_names_rule_field_and_pattern() {
        let error = Config::from_toml(
            r#"
[sticky.discord]
app-id = "discord"
exclude-app-id = "^(Discord$"
"#,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("Invalid exclude-app-id regex in sticky.discord")
                && message.contains("\"^(Discord$\""),
            "{message}"
        );
    }

    #[test]
    fn test_scratchpad_entry_that_is_not_a_table_is_skipped() {
        let config = config_from_toml(
            r#"
scratchpad = { broken = 3, term = { app-id = "foot" } }
"#,
        );
        assert_eq!(config.scratchpad_names(), vec!["term"]);
        assert!(config.scratchpad("term").is_some());
    }

    #[test]
    fn test_rule_entry_that_is_not_a_table_is_skipped() {
        let config = config_from_toml(
            r#"
sticky = { broken = 3, firefox = { app-id = "firefox" } }
"#,
        );
        assert_eq!(config.rules().len(), 1);
        assert_eq!(config.rules()[0].1.id, "sticky.firefox");
        assert!(matched(&config, &app("firefox"), &None));
    }

    #[test]
    fn test_menu_array_with_a_non_string_entry_is_reported() {
        let error = Config::from_toml("menu = [\"rofi\", 3]\n").unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("Invalid menu entry") && message.contains("got 3"),
            "{message}"
        );
    }

    #[test]
    fn test_empty_pattern_array_narrows_nothing() {
        // An empty pattern list narrows nothing, so the rule is dropped.
        let config = config_from_toml(
            r#"
[sticky.by-title]
app-id = []
title = "^Gmail$"

[sticky.empty]
app-id = []
"#,
        );
        assert_eq!(config.rules().len(), 1);
        assert_eq!(config.rules()[0].1.id, "sticky.by-title");
        assert!(matched(&config, &app("anything"), &title("Gmail")));
        assert!(!matched(&config, &app("anything"), &title("Home")));
    }

    #[test]
    fn test_empty_string_pattern_matches_any_reported_app_id() {
        // An empty regex matches everything, but a window niri reports without
        // an app id still cannot satisfy the constraint.
        let config = config_from_toml(
            r#"
[sticky.any]
app-id = ""
"#,
        );
        assert!(matched(&config, &app("firefox"), &None));
        assert!(matched(&config, &app(""), &None));
        assert!(!matched(&config, &None, &None));
    }

    #[test]
    fn test_not_aliases_accept_the_underscore_spelling() {
        let config = config_from_toml(
            r#"
[sticky.firefox]
app-id = "firefox"
not_title = "Gmail"
not_app_id = "firefox-esr"
"#,
        );
        assert!(matched(&config, &app("firefox"), &title("YouTube")));
        assert!(!matched(&config, &app("firefox"), &title("Gmail")));
        assert!(!matched(&config, &app("firefox-esr"), &title("YouTube")));
    }

    #[test]
    fn test_config_dir_falls_back_when_there_is_no_config_home() {
        assert_eq!(
            Config::config_dir_under(None),
            PathBuf::from("/tmp/nsticky/nsticky")
        );
        assert_eq!(
            Config::config_dir_under(Some(PathBuf::from("/home/x/.config"))),
            PathBuf::from("/home/x/.config/nsticky")
        );
    }

    /// A path in the system temp directory, never the user's configuration.
    fn temp_config_path(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nsticky-config-test-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("config.toml")
    }

    #[test]
    fn test_malformed_toml_does_not_become_a_default_config() {
        let error = Config::from_toml("stage-workspace = \"a\"\n[sticky\n").unwrap_err();
        assert!(
            format!("{error:#}").contains("TOML parse error"),
            "{error:#}"
        );
    }

    #[test]
    fn test_load_reads_a_file_and_names_it_when_it_is_broken() {
        let path = temp_config_path("load");
        std::fs::write(&path, "[sticky.firefox]\napp-id = \"firefox\"\n").unwrap();
        let config = Config::load(&path).expect("valid config");
        assert!(matched(&config, &app("firefox"), &None));

        std::fs::write(&path, "[sticky.firefox\n").unwrap();
        let message = format!("{:#}", Config::load(&path).unwrap_err());
        assert!(message.contains("Failed to parse TOML"), "{message}");
        assert!(message.contains(&path.display().to_string()), "{message}");

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn test_load_or_default_keeps_a_usable_file_and_falls_back_otherwise() {
        let path = temp_config_path("or-default");

        std::fs::write(&path, "[sticky.firefox\n").unwrap();
        let fallback = Config::load_or_default_at(&path);
        assert!(!matched(&fallback, &app("firefox"), &None));
        assert_eq!(fallback.stage_workspace(), "stage", "defaults are used");

        std::fs::write(&path, "stage-workspace = \"parking\"\n").unwrap();
        assert_eq!(
            Config::load_or_default_at(&path).stage_workspace(),
            "parking"
        );

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
