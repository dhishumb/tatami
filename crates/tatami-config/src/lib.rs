//! tatami's configuration: `config.toml` and the keymap presets.
//!
//! `~/.config/tatami/config.toml` (or `$XDG_CONFIG_HOME/tatami/…`, or
//! `$TATAMI_CONFIG`) is read once at start and on `reload-config`. A
//! missing file is all defaults; a TOML error is a `ConfigError` naming
//! the file, so the daemon can keep the config it already has. Keys are
//! more forgiving still: a bad binding is a warning and is skipped, never
//! a failed load.

mod keys;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tatami_keymap::{Keymap, Mods};
use tatami_protocol::{Action, Layout};

pub use keys::{
    BUILTIN_KEYMAPS, Binds, DEFAULT_PRESET, KeymapFile, Keys, build_keymap, builtin_keymap,
    command_words, describe, mode_action, parse_keymap_file, resolve_keymap,
};

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub gaps: Gaps,
    pub layout: LayoutConfig,
    pub workspaces: Workspaces,
    pub restore: Restore,
    pub rules: Vec<Rule>,
    #[serde(deserialize_with = "keys::de_keys")]
    pub keys: Keys,
    /// Problems that didn't fail the load (a rule that matches nothing).
    /// Keymap warnings come from `keymap()`.
    #[serde(skip)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct General {
    /// What `edit-config` opens the file with (`code`, `zed`, `vim` in a
    /// terminal of your choosing); empty opens TextEdit via `open -t`.
    pub editor: String,
    /// How long a pending prefix (alt held alone, or a mode chord) waits
    /// before the which-key sheet opens. `0` opens it at once.
    pub which_key_delay_ms: u64,
    /// The held modifier that makes a chord a binding outside a mode.
    #[serde(deserialize_with = "de_leader")]
    pub leader: Mods,
    /// When a key moves focus to another monitor, move the pointer there
    /// too, so the next chord acts on that monitor.
    pub mouse_follows_focus: bool,
    /// Where the `alt+o` overview opens: `main` (the primary display),
    /// `pointer`, `focused`, a monitor number (left to right, from 1) or
    /// part of a monitor's name.
    pub overview_monitor: String,
}

/// Long enough that a practised `alt+w 3` never flashes the sheet, short
/// enough that a pause shows it before you start wondering.
pub const WHICH_KEY_DELAY_MS: u64 = 300;

/// The starter `config.toml`: every option at its default, commented. The
/// installer seeds a missing config with it and `edit-config` writes it
/// before opening the editor, so there is always something to edit.
pub const CONFIG_TEMPLATE: &str = include_str!("../config.toml");

impl Default for General {
    fn default() -> Self {
        General {
            editor: String::new(),
            which_key_delay_ms: WHICH_KEY_DELAY_MS,
            leader: Mods::ALT,
            mouse_follows_focus: true,
            overview_monitor: "main".into(),
        }
    }
}

impl General {
    pub fn which_key_delay(&self) -> Duration {
        Duration::from_millis(self.which_key_delay_ms)
    }
}

fn de_leader<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Mods, D::Error> {
    let s = String::deserialize(d)?;
    Mods::parse(&s).ok_or_else(|| {
        serde::de::Error::custom(format!(
            "unknown leader `{s}` (try \"alt\" or \"ctrl+alt\")"
        ))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Gaps {
    /// Between neighbouring windows.
    pub inner: u32,
    /// Between the windows and the screen edge.
    pub outer: Outer,
}

impl Default for Gaps {
    fn default() -> Self {
        Gaps {
            inner: DEFAULT_GAP,
            outer: Outer::default(),
        }
    }
}

const DEFAULT_GAP: u32 = 8;

/// `outer = 8` or `outer = { top = 8, bottom = 8, left = 8, right = 8 }`;
/// a side left out of the table keeps the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(from = "OuterRaw")]
pub struct Outer {
    pub top: u32,
    pub bottom: u32,
    pub left: u32,
    pub right: u32,
}

impl Outer {
    pub const fn all(n: u32) -> Outer {
        Outer {
            top: n,
            bottom: n,
            left: n,
            right: n,
        }
    }
}

impl Default for Outer {
    fn default() -> Self {
        Outer::all(DEFAULT_GAP)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OuterRaw {
    All(u32),
    Sides {
        top: Option<u32>,
        bottom: Option<u32>,
        left: Option<u32>,
        right: Option<u32>,
    },
}

impl From<OuterRaw> for Outer {
    fn from(r: OuterRaw) -> Outer {
        match r {
            OuterRaw::All(n) => Outer::all(n),
            OuterRaw::Sides {
                top,
                bottom,
                left,
                right,
            } => Outer {
                top: top.unwrap_or(DEFAULT_GAP),
                bottom: bottom.unwrap_or(DEFAULT_GAP),
                left: left.unwrap_or(DEFAULT_GAP),
                right: right.unwrap_or(DEFAULT_GAP),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct LayoutConfig {
    /// Root layout of a new workspace.
    pub default: Layout,
    /// How much of each hidden accordion window peeks out, in points.
    pub accordion_padding: u32,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        LayoutConfig {
            default: Layout::HTiles,
            accordion_padding: 30,
        }
    }
}

/// Where a new window goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NewWindow {
    /// Its own (lowest empty) workspace on its monitor, alone, so it fills
    /// the screen; tiling is a deliberate move into another workspace.
    #[default]
    OwnWorkspace,
    /// Beside the focused tile in the visible workspace, i3-style.
    Tile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceModel {
    /// Each monitor has its own set of workspaces (`1` on every screen).
    #[default]
    PerMonitor,
    /// One global set shown on whichever monitor, AeroSpace's model.
    Shared,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Workspaces {
    pub model: WorkspaceModel,
    pub names: Vec<String>,
    /// Never garbage-collected when empty.
    pub persistent: Vec<String>,
    /// `own-workspace` (default) or `tile`.
    pub new_window: NewWindow,
    /// Shared model only, AeroSpace's syntax: `name = pattern` or
    /// `name = [patterns]`, a pattern being `main`, `secondary`, a 1-based
    /// index or a monitor-name regex. The first monitor present wins.
    #[serde(
        rename = "monitor-force-assignment",
        deserialize_with = "de_assignments"
    )]
    pub monitor_force_assignment: BTreeMap<String, Vec<String>>,
}

impl Default for Workspaces {
    fn default() -> Self {
        Workspaces {
            model: WorkspaceModel::default(),
            names: (1..=9).map(|n| n.to_string()).collect(),
            persistent: Vec::new(),
            new_window: NewWindow::default(),
            monitor_force_assignment: BTreeMap::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Pattern {
    Text(String),
    Index(i64),
}

impl Pattern {
    fn into_string(self) -> String {
        match self {
            Pattern::Text(s) => s,
            Pattern::Index(n) => n.to_string(),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(Pattern),
    Many(Vec<Pattern>),
}

fn de_assignments<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, Vec<String>>, D::Error> {
    let raw = BTreeMap::<String, OneOrMany>::deserialize(d)?;
    Ok(raw
        .into_iter()
        .map(|(ws, v)| {
            let pats = match v {
                OneOrMany::One(p) => vec![p.into_string()],
                OneOrMany::Many(ps) => ps.into_iter().map(Pattern::into_string).collect(),
            };
            (ws, pats)
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Restore {
    /// Put windows back where they were after a restart or crash.
    pub enabled: bool,
    /// Relaunch apps that were running at the checkpoint.
    pub launch_apps: bool,
    /// Stop waiting for relaunched apps' windows after this long.
    pub timeout_s: u64,
    /// Bundle ids never relaunched.
    pub ignore: Vec<String>,
}

impl Default for Restore {
    fn default() -> Self {
        Restore {
            enabled: true,
            launch_apps: true,
            timeout_s: 60,
            ignore: Vec::new(),
        }
    }
}

/// `[[rules]]`: AeroSpace's on-window-detected, flattened. Every matcher
/// given must match; the first matching rule wins. Regexes are kept as
/// text here and compiled by whoever matches windows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Rule {
    pub app_id: Option<String>,
    pub app_name_regex: Option<String>,
    pub title_regex: Option<String>,
    pub float: bool,
    pub workspace: Option<String>,
    /// Leave the window entirely alone.
    pub ignore: bool,
}

impl Rule {
    fn has_matcher(&self) -> bool {
        self.app_id.is_some() || self.app_name_regex.is_some() || self.title_regex.is_some()
    }

    fn has_effect(&self) -> bool {
        self.float || self.ignore || self.workspace.is_some()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{}: {message}", path.display())]
    Parse { path: PathBuf, message: String },
}

impl Config {
    /// Parse config text. `path` only labels errors.
    pub fn parse(text: &str, path: &Path) -> Result<Config, ConfigError> {
        let mut cfg: Config = toml::from_str(text).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        let mut warnings = Vec::new();
        cfg.rules.retain(|r| {
            let problem = match (r.has_matcher(), r.has_effect()) {
                (false, _) => "matches nothing (needs app_id, app_name_regex or title_regex)",
                (_, false) => "does nothing (needs float, workspace or ignore)",
                _ => return true,
            };
            warnings.push(format!(
                "{}: [[rules]] {r:?} {problem}; skipped",
                path.display()
            ));
            false
        });
        for w in &warnings {
            tracing::warn!("{w}");
        }
        cfg.warnings = warnings;
        Ok(cfg)
    }

    /// The user's rules, then the built-in ones (so the user's win).
    pub fn rules_in_effect(&self) -> impl Iterator<Item = &Rule> {
        self.rules.iter().chain(BUILTIN_RULES.iter())
    }

    /// The keymap this config describes, plus every warning met building
    /// it (also logged).
    pub fn keymap(&self) -> (Keymap<Action>, Vec<String>) {
        build_keymap(&self.keys, self.general.leader)
    }
}

/// Load `path`. A missing file is all defaults, not an error.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Config::parse(&text, path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(source) => Err(ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Apps whose main window has a fixed width or is a utility panel: tiling
/// them pins the row to their size, so they float. Consulted after the
/// user's rules, which therefore win (an `ignore` or a `workspace` rule for
/// the same app takes precedence). See `Config::rules_in_effect`.
pub static BUILTIN_RULES: std::sync::LazyLock<Vec<Rule>> = std::sync::LazyLock::new(builtin_rules);

fn builtin_rules() -> Vec<Rule> {
    [
        "com.apple.systempreferences",
        "com.apple.calculator",
        "com.apple.archiveutility",
        "com.apple.installer",
        "com.apple.DigitalColorMeter",
    ]
    .into_iter()
    .map(|id| Rule {
        app_id: Some(id.to_string()),
        app_name_regex: None,
        title_regex: None,
        float: true,
        workspace: None,
        ignore: false,
    })
    .collect()
}

/// `$TATAMI_CONFIG`, else `$XDG_CONFIG_HOME/tatami/config.toml`, else
/// `~/.config/tatami/config.toml` — unless only `~/.tatami/config.toml`
/// exists, which is then used.
pub fn config_path() -> PathBuf {
    config_path_from(|k| std::env::var_os(k), dirs::home_dir())
}

fn config_path_from(env: impl Fn(&str) -> Option<OsString>, home: Option<PathBuf>) -> PathBuf {
    if let Some(p) = env("TATAMI_CONFIG").filter(|p| !p.is_empty()) {
        return p.into();
    }
    let home = home.unwrap_or_else(std::env::temp_dir);
    let base = env("XDG_CONFIG_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let primary = base.join("tatami").join("config.toml");
    let legacy = home.join(".tatami").join("config.toml");
    if !primary.exists() && legacy.exists() {
        legacy
    } else {
        primary
    }
}

/// `keymaps/` beside `config.toml`, wherever that resolved to.
pub fn keymaps_dir() -> PathBuf {
    config_path()
        .parent()
        .map(|p| p.join("keymaps"))
        .unwrap_or_else(|| PathBuf::from("keymaps"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tatami_keymap::{KeyChord, Level};
    use tatami_protocol::action::ResizeDelta;
    use tatami_protocol::{Dir, MonitorTarget, ResizeDim, WorkspaceTarget};

    fn c(s: &str) -> KeyChord {
        KeyChord::parse(s).unwrap_or_else(|| panic!("chord {s}"))
    }

    fn parse(text: &str) -> Config {
        Config::parse(text, Path::new("test.toml")).unwrap_or_else(|e| panic!("{e}"))
    }

    fn ws(n: &str) -> Action {
        Action::Workspace {
            target: WorkspaceTarget::Name(n.into()),
            wrap: false,
        }
    }

    /// A scratch directory unique to this test.
    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tatami-config-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn preset() -> (Keymap<Action>, Vec<String>) {
        keys::build_keymap_in(&Keys::default(), Mods::ALT, None)
    }

    #[test]
    fn empty_text_is_all_defaults() {
        let cfg = parse("");
        assert_eq!(cfg, Config::default());
        assert_eq!(cfg.general.which_key_delay(), Duration::from_millis(300));
        assert_eq!(cfg.general.leader, Mods::ALT);
        assert_eq!(
            cfg.gaps,
            Gaps {
                inner: 8,
                outer: Outer::all(8)
            }
        );
        assert_eq!(cfg.layout.default, Layout::HTiles);
        assert_eq!(cfg.workspaces.model, WorkspaceModel::PerMonitor);
        assert_eq!(cfg.workspaces.names.len(), 9);
        assert!(cfg.restore.enabled && cfg.restore.launch_apps);
        assert_eq!(cfg.keys.preset, "tatami");
    }

    #[test]
    fn the_template_is_the_defaults_spelled_out() {
        // Every line in it is a default, so a fresh file changes nothing;
        // a default that drifts from the template fails here.
        let cfg = parse(CONFIG_TEMPLATE);
        assert_eq!(cfg, Config::default());
        assert!(cfg.warnings.is_empty());
    }

    #[test]
    fn the_documented_sample_parses() {
        let cfg = parse(
            r#"
[general]
editor = "code"
which_key_delay_ms = 250
leader = "alt"
[gaps]
inner = 6
outer = { top = 10, bottom = 8, left = 8, right = 8 }
[layout]
default = "v_accordion"
accordion_padding = 20
[workspaces]
model = "shared"
names = ["1","2","3","web"]
persistent = ["web"]
[workspaces.monitor-force-assignment]
"1" = "main"
"2" = ["secondary", "dell"]
"3" = 2
[restore]
enabled = true
launch_apps = false
timeout_s = 30
ignore = ["com.apple.Terminal"]
[[rules]]
app_id = "com.apple.systempreferences"
float = true
[[rules]]
title_regex = "Picture in Picture"
ignore = true
[[rules]]
app_name_regex = "^Slack$"
workspace = "3"
[keys]
preset = "tatami"
"alt+d" = "exec-and-forget open -a Terminal"
[keys.modes.resize]
"x" = "balance-sizes"
"#,
        );
        assert_eq!(cfg.general.which_key_delay_ms, 250);
        assert_eq!(cfg.gaps.inner, 6);
        assert_eq!(cfg.gaps.outer.top, 10);
        assert_eq!(cfg.layout.default, Layout::VAccordion);
        assert_eq!(cfg.layout.accordion_padding, 20);
        assert_eq!(cfg.workspaces.model, WorkspaceModel::Shared);
        assert_eq!(cfg.workspaces.persistent, ["web"]);
        let fa = &cfg.workspaces.monitor_force_assignment;
        assert_eq!(fa["1"], ["main"]);
        assert_eq!(fa["2"], ["secondary", "dell"]);
        assert_eq!(fa["3"], ["2"]);
        assert!(!cfg.restore.launch_apps);
        assert_eq!(cfg.restore.timeout_s, 30);
        assert_eq!(cfg.rules.len(), 3);
        assert!(cfg.rules[0].float);
        assert_eq!(cfg.rules[2].workspace.as_deref(), Some("3"));
        assert!(cfg.warnings.is_empty());
        assert_eq!(cfg.keys.preset, "tatami");
        assert_eq!(cfg.keys.overrides.direct.len(), 1);
    }

    #[test]
    fn outer_gap_takes_a_single_number_or_partial_sides() {
        assert_eq!(parse("[gaps]\nouter = 12").gaps.outer, Outer::all(12));
        let o = parse("[gaps]\nouter = { left = 0 }").gaps.outer;
        assert_eq!(
            o,
            Outer {
                left: 0,
                ..Outer::all(8)
            }
        );
    }

    #[test]
    fn errors_name_the_file() {
        let e = Config::parse("[general]\nleader = \"hyper\"", Path::new("/x/config.toml"))
            .unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.starts_with("/x/config.toml: ") && msg.contains("unknown leader"),
            "{msg}"
        );
        let e = Config::parse("[gaps\ninner = 1", Path::new("/x/config.toml")).unwrap_err();
        assert!(matches!(e, ConfigError::Parse { .. }));
        let e = Config::parse("[layout]\ndefault = \"spiral\"", Path::new("c.toml")).unwrap_err();
        assert!(e.to_string().contains("spiral"), "{e}");
    }

    #[test]
    fn a_missing_file_is_all_defaults() {
        let dir = scratch("missing");
        assert_eq!(load(&dir.join("config.toml")).unwrap(), Config::default());
        std::fs::write(dir.join("config.toml"), "[gaps]\ninner = 3").unwrap();
        assert_eq!(load(&dir.join("config.toml")).unwrap().gaps.inner, 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rules_without_a_matcher_or_effect_are_dropped_with_a_warning() {
        let cfg = parse(
            "[[rules]]\nfloat = true\n[[rules]]\napp_id = \"x\"\n[[rules]]\napp_id = \"y\"\nfloat = true",
        );
        assert_eq!(cfg.rules.len(), 1);
        assert_eq!(cfg.rules[0].app_id.as_deref(), Some("y"));
        assert_eq!(cfg.warnings.len(), 2);
    }

    #[test]
    fn the_preset_loads_clean_and_every_mode_it_names_exists() {
        let (km, warnings) = preset();
        assert!(warnings.is_empty(), "{warnings:#?}");
        let mut entered = 0;
        for (_, chord, actions) in km.bindings() {
            assert!(!actions.is_empty(), "{chord}");
            for a in actions {
                if let Action::Mode(m) = a {
                    assert!(km.has_mode(m), "`{chord}` enters missing mode {m}");
                    entered += 1;
                }
            }
        }
        assert_eq!(entered, 4);
        for m in ["workspaces", "move", "resize", "service"] {
            assert!(km.has_mode(m));
            // Esc / Enter leave every mode.
            assert_eq!(
                km.lookup_mode(m, &c("esc")),
                Some(&[Action::Normal][..]),
                "{m}"
            );
            assert_eq!(
                km.lookup_mode(m, &c("enter")),
                Some(&[Action::Normal][..]),
                "{m}"
            );
        }
    }

    #[test]
    fn the_preset_binds_what_the_docs_say() {
        let (km, _) = preset();
        assert_eq!(
            km.lookup_direct(&c("alt+h")),
            Some(
                &[Action::Focus {
                    dir: Dir::Left,
                    wrap: false
                }][..]
            )
        );
        assert_eq!(
            km.lookup_direct(&c("alt+shift+l")),
            Some(&[Action::Move { dir: Dir::Right }][..])
        );
        assert_eq!(km.lookup_direct(&c("alt+3")), Some(&[ws("3")][..]));
        assert_eq!(
            km.lookup_direct(&c("alt+shift+3")),
            Some(
                &[
                    Action::MoveNodeToWorkspace {
                        target: WorkspaceTarget::Name("3".into()),
                        focus_follows: false,
                        wrap: false
                    },
                    ws("3")
                ][..]
            )
        );
        assert_eq!(km.lookup_direct(&c("alt+/")), Some(&[Action::Help][..]));
        assert!(
            km.lookup_direct(&c("alt+enter")).is_none(),
            "apps keep alt+enter"
        );
        assert_eq!(
            km.lookup_mode("workspaces", &c("3")),
            Some(&[ws("3"), Action::Normal][..])
        );
        assert_eq!(
            km.lookup_mode("move", &c("shift+h")),
            Some(
                &[Action::MoveNodeToMonitor {
                    target: MonitorTarget::Dir(Dir::Left),
                    focus_follows: true,
                    wrap: false
                }][..]
            )
        );
        // `+` is shift+= on the keycode the tap reports.
        assert_eq!(
            km.lookup_mode(
                "resize",
                &KeyChord::new(tatami_keymap::KeyName::Char('='), Mods::SHIFT)
            ),
            Some(
                &[Action::Resize {
                    dim: ResizeDim::Smart,
                    delta: ResizeDelta::Add(50.0)
                }][..]
            )
        );
        assert_eq!(
            km.lookup_mode("resize", &c("=")),
            Some(&[Action::BalanceSizes][..])
        );
        assert_eq!(
            km.lookup_mode("service", &c("q")),
            Some(&[Action::Quit, Action::Normal][..])
        );
    }

    #[test]
    fn builtin_float_rules_apply_after_the_users() {
        let cfg = Config::parse("", Path::new("x")).unwrap();
        assert!(
            cfg.rules.is_empty(),
            "built-ins are not stored in the config"
        );
        let settings = cfg
            .rules_in_effect()
            .find(|r| r.app_id.as_deref() == Some("com.apple.systempreferences"))
            .expect("System Settings floats by default");
        assert!(settings.float && !settings.ignore);
        let cfg = Config::parse(
            "[[rules]]\napp_id = \"com.apple.systempreferences\"\nignore = true\n",
            Path::new("x"),
        )
        .unwrap();
        let first = cfg
            .rules_in_effect()
            .find(|r| r.app_id.as_deref() == Some("com.apple.systempreferences"))
            .unwrap();
        assert!(first.ignore, "the user's rule comes first and wins");
    }

    #[test]
    fn the_preset_sheet_reads_well() {
        let (km, _) = preset();
        let top = km.which_key(&Level::Top);
        let row = |k: &str| {
            top.iter()
                .find(|e| e.key == k)
                .unwrap_or_else(|| panic!("{k} in {top:#?}"))
        };
        assert_eq!(row("h").desc, "focus left");
        assert_eq!(row("1-9").desc, "workspace 1-9");
        assert_eq!(row("shift+1-9").desc, "move-node-to-workspace 1-9");
        assert_eq!(
            (row("w").desc.as_str(), row("w").prefix),
            ("+workspaces", true)
        );
        assert_eq!(row("e").desc, "flip");
        assert_eq!(row("E").desc, "layout tiles horizontal vertical");
        let resize = km.which_key(&Level::Mode("resize".into()));
        assert!(
            resize
                .iter()
                .any(|e| e.key == "+" && e.desc == "resize smart +50")
        );
        assert!(resize.iter().all(|e| e.key != "esc" && e.key != "enter"));
        assert_eq!(km.mode_path("move"), vec![c("alt+m")]);
    }

    #[test]
    fn keys_overrides_replace_and_add() {
        let cfg = parse(
            r#"
[keys]
"alt+h" = "focus right"
"alt-d" = "exec-and-forget open -a Terminal"
"alt+f" = []
[keys.modes.resize]
"x" = ["balance-sizes", "normal"]
[keys.modes.launch]
"t" = ["exec-and-forget open -a Terminal", "normal"]
"#,
        );
        let (km, warnings) = keys::build_keymap_in(&cfg.keys, cfg.general.leader, None);
        assert!(warnings.is_empty(), "{warnings:#?}");
        assert_eq!(
            km.lookup_direct(&c("alt+h")),
            Some(
                &[Action::Focus {
                    dir: Dir::Right,
                    wrap: false
                }][..]
            )
        );
        assert_eq!(
            km.lookup_direct(&c("alt+d")),
            Some(&[Action::Exec("open -a Terminal".into())][..])
        );
        assert!(km.lookup_direct(&c("alt+f")).is_none(), "[] unbinds");
        assert_eq!(
            km.lookup_mode("resize", &c("x")),
            Some(&[Action::BalanceSizes, Action::Normal][..])
        );
        // The preset's resize keys are still there.
        assert!(km.lookup_mode("resize", &c("h")).is_some());
        // A mode first declared in [keys] still gets the preset's Esc.
        assert_eq!(
            km.lookup_mode("launch", &c("esc")),
            Some(&[Action::Normal][..])
        );
    }

    #[test]
    fn bad_bindings_are_skipped_not_fatal() {
        let cfg = parse(
            r#"
[keys]
"hyper+x" = "close"
"alt+y" = "frobnicate"
"alt+u" = 5
"cmd+x" = "close"
"alt+z" = "mode nowhere"
"alt+o" = "close"
"#,
        );
        let (km, warnings) = keys::build_keymap_in(&cfg.keys, cfg.general.leader, None);
        assert_eq!(warnings.len(), 5, "{warnings:#?}");
        assert!(km.lookup_direct(&c("alt+y")).is_none());
        assert_eq!(km.lookup_direct(&c("alt+o")), Some(&[Action::Close][..]));
        assert!(
            km.lookup_direct(&c("alt+h")).is_some(),
            "the preset survived"
        );
    }

    #[test]
    fn user_presets_extend_builtins_and_cycles_stop() {
        let dir = scratch("presets");
        std::fs::write(
            dir.join("mine.toml"),
            "extends = \"tatami\"\n\"alt+h\" = \"focus right\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("a.toml"),
            "extends = \"b\"\n\"alt+x\" = \"close\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("b.toml"), "extends = \"a\"\n").unwrap();
        let keys = |p: &str| Keys {
            preset: p.into(),
            overrides: KeymapFile::default(),
        };
        let (km, w) = keys::build_keymap_in(&keys("mine"), Mods::ALT, Some(&dir));
        assert!(w.is_empty(), "{w:#?}");
        assert_eq!(
            km.lookup_direct(&c("alt+h")),
            Some(
                &[Action::Focus {
                    dir: Dir::Right,
                    wrap: false
                }][..]
            )
        );
        assert!(km.lookup_direct(&c("alt+l")).is_some());
        let (km, w) = keys::build_keymap_in(&keys("a"), Mods::ALT, Some(&dir));
        assert!(w.iter().any(|w| w.contains("too deep")), "{w:#?}");
        assert_eq!(km.lookup_direct(&c("alt+x")), Some(&[Action::Close][..]));
        // An unknown preset falls back to the built-in.
        let (km, w) = keys::build_keymap_in(&keys("nope"), Mods::ALT, Some(&dir));
        assert_eq!(w.len(), 1);
        assert!(km.lookup_direct(&c("alt+h")).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn describe_renders_command_words() {
        assert_eq!(
            describe(&[Action::Mode("workspaces".into())]),
            "+workspaces"
        );
        assert_eq!(describe(&[ws("3"), Action::Normal]), "workspace 3");
        assert_eq!(describe(&[Action::Help]), "help");
        assert_eq!(describe(&[Action::Normal]), "normal");
        assert_eq!(describe(&[]), "");
        for line in [
            "focus left",
            "move-node-to-workspace 3",
            "resize width -50",
            "resize smart +50",
            "layout floating tiling",
            "fullscreen",
            "enable toggle",
            "focus-monitor next",
            "checkpoint save",
            "swap dfs-next",
            "exec-and-forget open -a Terminal",
        ] {
            assert_eq!(describe(&Action::parse_chain(line).unwrap()), line);
        }
        // Flags are dropped for display.
        assert_eq!(
            describe(
                &Action::parse_chain("move-node-to-monitor left --focus-follows-window").unwrap()
            ),
            "move-node-to-monitor left"
        );
    }

    #[test]
    fn config_path_resolution() {
        let dir = scratch("paths");
        let home = dir.join("home");
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| OsString::from(*v))
            }
        };
        assert_eq!(
            config_path_from(env(&[("TATAMI_CONFIG", "/etc/t.toml")]), Some(home.clone())),
            PathBuf::from("/etc/t.toml")
        );
        assert_eq!(
            config_path_from(env(&[("XDG_CONFIG_HOME", "/xdg")]), Some(home.clone())),
            PathBuf::from("/xdg/tatami/config.toml")
        );
        assert_eq!(
            config_path_from(env(&[]), Some(home.clone())),
            home.join(".config/tatami/config.toml")
        );
        // Only the legacy file exists: use it.
        std::fs::create_dir_all(home.join(".tatami")).unwrap();
        std::fs::write(home.join(".tatami/config.toml"), "").unwrap();
        assert_eq!(
            config_path_from(env(&[]), Some(home.clone())),
            home.join(".tatami/config.toml")
        );
        // Both exist: the XDG one wins.
        std::fs::create_dir_all(home.join(".config/tatami")).unwrap();
        std::fs::write(home.join(".config/tatami/config.toml"), "").unwrap();
        assert_eq!(
            config_path_from(env(&[]), Some(home.clone())),
            home.join(".config/tatami/config.toml")
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
