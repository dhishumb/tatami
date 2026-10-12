//! Keymap files and the `[keys]` table, turned into a `Keymap<Action>`.
//!
//! A keymap file is plyterm's shape: an optional `extends` header, a
//! `passthrough` list, top-level `"chord" = "command"` lines, a
//! `[modes."*"]` table bound into every mode, and `[modes.<name>]`
//! tables. `[keys]` in config.toml is the same shape with `preset` in
//! place of `extends`, applied last.
//!
//! Everything is parsed here, once: chords to `KeyChord`, command lines
//! to `Action`s. A bad entry is logged, collected as a warning and
//! skipped — a typo must never cost you the rest of your keyboard.

use std::path::Path;

use serde::Deserialize;
use tatami_keymap::{KeyChord, Keymap, ModeAction, Mods};
use tatami_protocol::Action;
use tatami_protocol::action::{CheckpointCmd, ResizeDelta, SplitArg, Toggle};
use tatami_protocol::{LayoutArg, MonitorTarget, ResizeDim, WorkspaceTarget};

/// `[(chord, command lines)]`, in file order.
pub type Binds = Vec<(String, Vec<String>)>;

/// One keymap file, chords and commands still as text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeymapFile {
    /// Preset to load first; this file's bindings go on top.
    pub extends: Option<String>,
    /// Modes whose unbound keys reach the app.
    pub passthrough: Vec<String>,
    /// `[modes."*"]`: bound into every non-passthrough mode, before the
    /// mode's own table so specific entries win.
    pub every_mode: Binds,
    pub direct: Binds,
    pub modes: Vec<(String, Binds)>,
    /// Shape problems found while reading, reported when applied.
    pub warnings: Vec<String>,
}

/// `[keys]`: a preset plus overrides.
#[derive(Debug, Clone, PartialEq)]
pub struct Keys {
    pub preset: String,
    pub overrides: KeymapFile,
}

impl Default for Keys {
    fn default() -> Self {
        Keys {
            preset: DEFAULT_PRESET.to_string(),
            overrides: KeymapFile::default(),
        }
    }
}

pub const DEFAULT_PRESET: &str = "tatami";

/// Names of the built-in presets, for `tatami keymaps`.
pub const BUILTIN_KEYMAPS: &[&str] = &[DEFAULT_PRESET];

/// Built-in presets, embedded so a bare binary still has them.
pub fn builtin_keymap(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "tatami" => Some(include_str!("../keymaps/tatami.toml")),
        _ => None,
    }
}

/// Find a preset's text: a built-in first (so a user file named
/// `tatami.toml` that `extends = "tatami"` is not its own parent), then
/// `<keymaps_dir>/<name>.toml`, then `name` as a path (`~` expands).
/// Returns `(where it came from, text)`.
pub fn resolve_keymap(name: &str) -> Option<(String, String)> {
    resolve_keymap_in(name, Some(&crate::keymaps_dir()))
}

fn resolve_keymap_in(name: &str, user_dir: Option<&Path>) -> Option<(String, String)> {
    if let Some(text) = builtin_keymap(name) {
        return Some((format!("built-in {name}"), text.to_string()));
    }
    if let Some(dir) = user_dir {
        let user = dir.join(format!("{name}.toml"));
        if let Ok(text) = std::fs::read_to_string(&user) {
            return Some((user.display().to_string(), text));
        }
    }
    let path = match name.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()?.join(rest),
        None => name.into(),
    };
    if path.extension().is_some() || path.is_absolute() || name.contains('/') {
        let text = std::fs::read_to_string(&path).ok()?;
        return Some((path.display().to_string(), text));
    }
    None
}

/// Parse a keymap file's TOML. Only a TOML syntax error fails; anything
/// shaped wrong inside is a warning on the result.
pub fn parse_keymap_file(text: &str) -> Result<KeymapFile, String> {
    let table: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
    Ok(from_table(&table))
}

/// `"command"` or `["command", "command"]` → list.
fn action_list(v: &toml::Value) -> Option<Vec<String>> {
    match v {
        toml::Value::String(s) => Some(vec![s.clone()]),
        toml::Value::Array(a) => a.iter().map(|x| x.as_str().map(String::from)).collect(),
        _ => None,
    }
}

fn binds(table: &toml::Table, ctx: &str, warnings: &mut Vec<String>) -> Binds {
    let mut out = Vec::new();
    for (chord, v) in table {
        match action_list(v) {
            Some(l) => out.push((chord.clone(), l)),
            None => warnings.push(format!(
                "{ctx}`{chord}`: value must be a string or a list of strings; skipped"
            )),
        }
    }
    out
}

fn from_table(table: &toml::Table) -> KeymapFile {
    let mut f = KeymapFile::default();
    let mut direct = toml::Table::new();
    for (k, val) in table {
        match k.as_str() {
            // `preset` is `[keys]`'s spelling of the same thing.
            "extends" | "preset" => match val.as_str() {
                Some(s) => f.extends = Some(s.to_string()),
                None => f.warnings.push(format!("`{k}` must be a string")),
            },
            "passthrough" => match val.as_array() {
                Some(a) => {
                    f.passthrough = a
                        .iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                }
                None => f
                    .warnings
                    .push("`passthrough` must be a list of mode names".into()),
            },
            "modes" => {
                let Some(modes) = val.as_table() else {
                    f.warnings.push("[modes] must be a table".into());
                    continue;
                };
                for (mode, t) in modes {
                    let Some(t) = t.as_table() else {
                        f.warnings.push(format!("[modes.{mode}] must be a table"));
                        continue;
                    };
                    let list = binds(t, &format!("[modes.{mode}] "), &mut f.warnings);
                    if mode == "*" {
                        f.every_mode = list;
                    } else {
                        f.modes.push((mode.clone(), list));
                    }
                }
            }
            _ => {
                direct.insert(k.clone(), val.clone());
            }
        }
    }
    f.direct = binds(&direct, "", &mut f.warnings);
    f
}

/// serde hook for `Config::keys`.
pub(crate) fn de_keys<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Keys, D::Error> {
    let table = toml::Table::deserialize(d)?;
    let mut overrides = from_table(&table);
    let preset = overrides
        .extends
        .take()
        .unwrap_or_else(|| DEFAULT_PRESET.to_string());
    Ok(Keys { preset, overrides })
}

/// Build the keymap: the preset (and whatever it extends), then `[keys]`.
/// Returns the map and every warning, which are also logged.
pub fn build_keymap(keys: &Keys, leader: Mods) -> (Keymap<Action>, Vec<String>) {
    build_keymap_in(keys, leader, Some(&crate::keymaps_dir()))
}

pub(crate) fn build_keymap_in(
    keys: &Keys,
    leader: Mods,
    user_dir: Option<&Path>,
) -> (Keymap<Action>, Vec<String>) {
    let mut km = Keymap::new(
        describe as fn(&[Action]) -> String,
        mode_action as fn(&Action) -> Option<ModeAction>,
    );
    km.set_leader(leader);
    let mut b = Builder {
        km,
        warnings: Vec::new(),
        every: Vec::new(),
        user_dir,
    };
    if !b.apply_preset(&keys.preset, 0) {
        // Anything unresolvable falls back to the built-in so a typo
        // never leaves the window manager keyless.
        b.warn(format!(
            "keys: preset `{}` not found; using the built-in {DEFAULT_PRESET}",
            keys.preset
        ));
        b.apply_preset(DEFAULT_PRESET, 0);
    }
    b.apply_file(&keys.overrides, "[keys]");
    b.check_modes();
    (b.km, b.warnings)
}

struct Builder<'a> {
    km: Keymap<Action>,
    warnings: Vec<String>,
    /// Every `[modes."*"]` binding so far, so a mode first declared by a
    /// later file (or by `[keys]`) still gets Esc / Enter.
    every: Binds,
    user_dir: Option<&'a Path>,
}

impl Builder<'_> {
    fn warn(&mut self, msg: String) {
        tracing::warn!("{msg}");
        self.warnings.push(msg);
    }

    /// Resolve, parse and apply `name` and whatever it extends. False if
    /// it couldn't be found or parsed.
    fn apply_preset(&mut self, name: &str, depth: usize) -> bool {
        if depth > 8 {
            self.warn(format!(
                "keys: `extends` chain too deep at `{name}` (a cycle?)"
            ));
            return false;
        }
        let Some((source, text)) = resolve_keymap_in(name, self.user_dir) else {
            return false;
        };
        let file = match parse_keymap_file(&text) {
            Ok(f) => f,
            Err(e) => {
                self.warn(format!("keys: {source}: {e}"));
                return false;
            }
        };
        if let Some(base) = &file.extends
            && !self.apply_preset(base, depth + 1)
        {
            self.warn(format!("keys: {source}: `extends = \"{base}\"` not found"));
        }
        tracing::debug!(preset = name, %source, "keys: loaded preset");
        self.apply_file(&file, &source);
        true
    }

    /// Apply one file on top of whatever is bound already.
    fn apply_file(&mut self, file: &KeymapFile, source: &str) {
        for w in &file.warnings {
            self.warn(format!("keys: {source}: {w}"));
        }
        for m in &file.passthrough {
            self.km.set_passthrough(m);
        }
        for (chord, actions) in &file.direct {
            self.bind_str(None, chord, actions, source);
        }
        // Modes this file brings into being get the earlier files' `*`
        // first; then this file's `*` goes into every mode, inherited and
        // new. Both before the mode tables, so specific keys win.
        let every = std::mem::take(&mut self.every);
        let mut targets: Vec<String> = self.km.modes().to_vec();
        for (m, _) in &file.modes {
            if !self.km.has_mode(m) {
                self.km.add_mode(m);
                targets.push(m.clone());
                for (chord, actions) in &every {
                    self.bind_str(Some(m), chord, actions, source);
                }
            }
        }
        self.every = every;
        for m in targets {
            if self.km.is_passthrough(&m) {
                continue;
            }
            for (chord, actions) in &file.every_mode {
                self.bind_str(Some(&m), chord, actions, source);
            }
        }
        self.every.extend(file.every_mode.iter().cloned());
        for (mode, binds) in &file.modes {
            for (chord, actions) in binds {
                self.bind_str(Some(mode), chord, actions, source);
            }
        }
    }

    fn bind_str(&mut self, mode: Option<&str>, chord: &str, lines: &[String], source: &str) {
        let Some(c) = KeyChord::parse(chord) else {
            self.warn(format!("keys: {source}: `{chord}` is not a chord; skipped"));
            return;
        };
        if lines.is_empty() {
            self.km.unbind(mode, &c);
            return;
        }
        let mut actions = Vec::new();
        for line in lines {
            match Action::parse_chain(line) {
                Ok(v) => actions.extend(v),
                Err(e) => {
                    self.warn(format!(
                        "keys: {source}: `{chord}` = \"{line}\": {e}; skipped"
                    ));
                    return;
                }
            }
        }
        if mode.is_none() && !c.mods.contains(self.km.leader()) {
            self.warn(format!(
                "keys: {source}: `{chord}` doesn't hold the leader, so normal mode never looks it up"
            ));
        }
        self.km.bind(mode, c, actions);
    }

    /// A `mode x` with no `[modes.x]` would be refused at the key press;
    /// say so at load instead.
    fn check_modes(&mut self) {
        let mut missing: Vec<String> = Vec::new();
        for (_, _, actions) in self.km.bindings() {
            for a in actions {
                if let Action::Mode(m) = a
                    && !self.km.has_mode(m)
                    && !missing.contains(m)
                {
                    missing.push(m.clone());
                }
            }
        }
        for m in missing {
            self.warn(format!(
                "keys: `mode {m}` is bound but [modes.{m}] binds nothing"
            ));
        }
    }
}

/// The key state applies these itself; see `Action::is_mode_change`.
pub fn mode_action(a: &Action) -> Option<ModeAction> {
    match a {
        Action::Mode(m) => Some(ModeAction::Enter(m.clone())),
        Action::Normal => Some(ModeAction::Normal),
        Action::Help => Some(ModeAction::Help),
        _ => None,
    }
}

/// A which-key row's text for a bound list: the first thing it *does*
/// (`move-node-to-workspace 3` for "move, then follow"), or `+name` when
/// all it does is open a mode.
pub fn describe(actions: &[Action]) -> String {
    if let Some(a) = actions.iter().find(|a| !a.is_mode_change()) {
        return command_words(a);
    }
    match actions
        .iter()
        .find(|a| !matches!(a, Action::Normal))
        .or(actions.first())
    {
        Some(Action::Mode(m)) => format!("+{m}"),
        Some(a) => command_words(a),
        None => String::new(),
    }
}

/// An action back in its command words, minus flags: `focus left`,
/// `resize width -50`, `layout floating tiling`. For display; the match
/// is exhaustive so a new command can't be forgotten here.
pub fn command_words(a: &Action) -> String {
    fn next(n: bool) -> &'static str {
        if n { "next" } else { "prev" }
    }
    fn ws(t: &WorkspaceTarget) -> &str {
        match t {
            WorkspaceTarget::Name(n) => n,
            WorkspaceTarget::Next => "next",
            WorkspaceTarget::Prev => "prev",
        }
    }
    fn mon(t: &MonitorTarget) -> String {
        match t {
            MonitorTarget::Dir(d) => d.to_string(),
            MonitorTarget::Next => "next".into(),
            MonitorTarget::Prev => "prev".into(),
            MonitorTarget::Pattern(p) => p.clone(),
        }
    }
    fn toggle(t: Toggle) -> &'static str {
        match t {
            Toggle::On => "on",
            Toggle::Off => "off",
            Toggle::Toggle => "toggle",
        }
    }
    match a {
        Action::Focus { dir, .. } => format!("focus {dir}"),
        Action::FocusDfs { next: n } => format!("focus dfs-{}", next(*n)),
        Action::FocusBackAndForth => "focus-back-and-forth".into(),
        Action::Move { dir } => format!("move {dir}"),
        Action::Swap { dir, .. } => format!("swap {dir}"),
        Action::SwapDfs { next: n } => format!("swap dfs-{}", next(*n)),
        Action::Resize { dim, delta } => {
            let dim = match dim {
                ResizeDim::Smart => "smart",
                ResizeDim::SmartOpposite => "smart-opposite",
                ResizeDim::Width => "width",
                ResizeDim::Height => "height",
            };
            match delta {
                ResizeDelta::Add(n) => format!("resize {dim} +{n}"),
                ResizeDelta::Sub(n) => format!("resize {dim} -{n}"),
                ResizeDelta::Set(n) => format!("resize {dim} {n}"),
            }
        }
        Action::Layout(args) => {
            let mut s = String::from("layout");
            for arg in args {
                s.push(' ');
                s.push_str(match arg {
                    LayoutArg::Floating => "floating",
                    LayoutArg::Tiling => "tiling",
                    LayoutArg::Tiles => "tiles",
                    LayoutArg::Accordion => "accordion",
                    LayoutArg::Horizontal => "horizontal",
                    LayoutArg::Vertical => "vertical",
                    LayoutArg::Exact(l) => l.name(),
                });
            }
            s
        }
        Action::Fullscreen(Toggle::Toggle) => "fullscreen".into(),
        Action::Fullscreen(t) => format!("fullscreen {}", toggle(*t)),
        Action::Workspace { target, .. } => format!("workspace {}", ws(target)),
        Action::WorkspaceBackAndForth => "workspace-back-and-forth".into(),
        Action::MoveNodeToWorkspace { target, .. } => {
            format!("move-node-to-workspace {}", ws(target))
        }
        Action::MoveNodeToMonitor { target, .. } => format!("move-node-to-monitor {}", mon(target)),
        Action::MoveWorkspaceToMonitor { target, .. } => {
            format!("move-workspace-to-monitor {}", mon(target))
        }
        Action::SummonWorkspace(n) => format!("summon-workspace {n}"),
        Action::FocusMonitor { target, .. } => format!("focus-monitor {}", mon(target)),
        Action::FlattenWorkspaceTree => "flatten-workspace-tree".into(),
        Action::JoinWith(d) => format!("join-with {d}"),
        Action::Flip => "flip".to_string(),
        Action::Overview => "overview".to_string(),
        Action::Split(s) => format!(
            "split {}",
            match s {
                SplitArg::Horizontal => "horizontal",
                SplitArg::Vertical => "vertical",
                SplitArg::Opposite => "opposite",
            }
        ),
        Action::BalanceSizes => "balance-sizes".into(),
        Action::Close => "close".into(),
        Action::CloseAllWindowsButCurrent => "close-all-windows-but-current".into(),
        Action::Mode(m) => format!("mode {m}"),
        Action::Normal => "normal".into(),
        Action::Exec(cmd) => format!("exec-and-forget {cmd}"),
        Action::ReloadConfig => "reload-config".into(),
        Action::Enable(t) => format!("enable {}", toggle(*t)),
        Action::ListWindows { .. } => "list-windows".into(),
        Action::ListWorkspaces { .. } => "list-workspaces".into(),
        Action::ListMonitors { .. } => "list-monitors".into(),
        Action::ListApps { .. } => "list-apps".into(),
        Action::ListModes { .. } => "list-modes".into(),
        Action::TriggerBinding { binding, .. } => format!("trigger-binding {binding}"),
        Action::Checkpoint(c) => match c {
            CheckpointCmd::Save(None) => "checkpoint save".into(),
            CheckpointCmd::Save(Some(n)) => format!("checkpoint save {n}"),
            CheckpointCmd::Restore(n) => format!("checkpoint restore {n}"),
            CheckpointCmd::List => "checkpoint list".into(),
            CheckpointCmd::Delete(n) => format!("checkpoint delete {n}"),
        },
        Action::Help => "help".into(),
        Action::Quit => "quit".into(),
        Action::Ping => "ping".into(),
        Action::Version => "version".into(),
        Action::Subscribe(topics) => format!("subscribe {}", topics.join(" ")),
        Action::DebugSetFrame { handle, .. } => format!("debug-set-frame {handle}"),
    }
}
