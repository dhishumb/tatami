//! The command vocabulary. Names follow AeroSpace so existing configs port
//! with a search-and-replace; the few extras (`checkpoint`, `help`, `quit`,
//! `normal`) are ours.
//!
//! Parsing happens once per config load or CLI request. Keybindings hold
//! already-parsed `Action`s, so a key press never re-parses text.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    Left,
    Down,
    Up,
    Right,
}

impl Dir {
    pub fn is_horizontal(self) -> bool {
        matches!(self, Dir::Left | Dir::Right)
    }
    pub fn axis(self) -> crate::geom::Axis {
        if self.is_horizontal() {
            crate::geom::Axis::H
        } else {
            crate::geom::Axis::V
        }
    }
    /// +1 toward right/down, -1 toward left/up.
    pub fn sign(self) -> i32 {
        match self {
            Dir::Left | Dir::Up => -1,
            Dir::Right | Dir::Down => 1,
        }
    }
    pub fn opposite(self) -> Dir {
        match self {
            Dir::Left => Dir::Right,
            Dir::Right => Dir::Left,
            Dir::Up => Dir::Down,
            Dir::Down => Dir::Up,
        }
    }
    fn parse(s: &str) -> Option<Dir> {
        Some(match s {
            "left" => Dir::Left,
            "down" => Dir::Down,
            "up" => Dir::Up,
            "right" => Dir::Right,
            _ => return None,
        })
    }
}

impl fmt::Display for Dir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Dir::Left => "left",
            Dir::Down => "down",
            Dir::Up => "up",
            Dir::Right => "right",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    HTiles,
    VTiles,
    HAccordion,
    VAccordion,
}

impl Layout {
    pub fn axis(self) -> crate::geom::Axis {
        match self {
            Layout::HTiles | Layout::HAccordion => crate::geom::Axis::H,
            Layout::VTiles | Layout::VAccordion => crate::geom::Axis::V,
        }
    }
    pub fn is_accordion(self) -> bool {
        matches!(self, Layout::HAccordion | Layout::VAccordion)
    }
    pub fn with_axis(self, axis: crate::geom::Axis) -> Layout {
        use crate::geom::Axis::*;
        match (self.is_accordion(), axis) {
            (false, H) => Layout::HTiles,
            (false, V) => Layout::VTiles,
            (true, H) => Layout::HAccordion,
            (true, V) => Layout::VAccordion,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Layout::HTiles => "h_tiles",
            Layout::VTiles => "v_tiles",
            Layout::HAccordion => "h_accordion",
            Layout::VAccordion => "v_accordion",
        }
    }
}

impl fmt::Display for Layout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One word of `layout a b c`: the command cycles to the first word that
/// differs from the current state, exactly as AeroSpace does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutArg {
    Floating,
    Tiling,
    Tiles,
    Accordion,
    Horizontal,
    Vertical,
    Exact(Layout),
}

impl LayoutArg {
    fn parse(s: &str) -> Option<LayoutArg> {
        Some(match s {
            "floating" => LayoutArg::Floating,
            "tiling" => LayoutArg::Tiling,
            "tiles" => LayoutArg::Tiles,
            "accordion" => LayoutArg::Accordion,
            "horizontal" => LayoutArg::Horizontal,
            "vertical" => LayoutArg::Vertical,
            "h_tiles" => LayoutArg::Exact(Layout::HTiles),
            "v_tiles" => LayoutArg::Exact(Layout::VTiles),
            "h_accordion" => LayoutArg::Exact(Layout::HAccordion),
            "v_accordion" => LayoutArg::Exact(Layout::VAccordion),
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceTarget {
    Name(String),
    Next,
    Prev,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorTarget {
    Dir(Dir),
    Next,
    Prev,
    /// `main`, `secondary`, a 1-based index, or a case-insensitive regex on
    /// the monitor name. Matched by the daemon.
    Pattern(String),
}

impl MonitorTarget {
    fn parse(s: &str) -> MonitorTarget {
        match s {
            "next" => MonitorTarget::Next,
            "prev" => MonitorTarget::Prev,
            _ => Dir::parse(s)
                .map(MonitorTarget::Dir)
                .unwrap_or_else(|| MonitorTarget::Pattern(s.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizeDim {
    Smart,
    SmartOpposite,
    Width,
    Height,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizeDelta {
    Add(f64),
    Sub(f64),
    Set(f64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitArg {
    Horizontal,
    Vertical,
    Opposite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Toggle {
    On,
    Off,
    Toggle,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointCmd {
    Save(Option<String>),
    Restore(String),
    List,
    Delete(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Focus {
        dir: Dir,
        wrap: bool,
    },
    /// `focus --window-id <handle>`: focus that window wherever it is.
    FocusWindow(u64),
    FocusDfs {
        next: bool,
    },
    FocusBackAndForth,
    Move {
        dir: Dir,
    },
    Swap {
        dir: Dir,
        wrap: bool,
    },
    SwapDfs {
        next: bool,
    },
    Resize {
        dim: ResizeDim,
        delta: ResizeDelta,
    },
    Layout(Vec<LayoutArg>),
    Fullscreen(Toggle),
    Workspace {
        target: WorkspaceTarget,
        wrap: bool,
    },
    WorkspaceBackAndForth,
    MoveNodeToWorkspace {
        target: WorkspaceTarget,
        focus_follows: bool,
        wrap: bool,
    },
    MoveNodeToMonitor {
        target: MonitorTarget,
        focus_follows: bool,
        wrap: bool,
    },
    MoveWorkspaceToMonitor {
        target: MonitorTarget,
        wrap: bool,
    },
    SummonWorkspace(String),
    FocusMonitor {
        target: MonitorTarget,
        wrap: bool,
    },
    FlattenWorkspaceTree,
    JoinWith(Dir),
    /// Pair the focused window with its neighbour along the parent's axis
    /// and stack the two the other way; on a pair, flip it back.
    Flip,
    Split(SplitArg),
    BalanceSizes,
    Close,
    CloseAllWindowsButCurrent,
    /// Enter a binding mode; `normal` leaves it (plyterm's word).
    Mode(String),
    Normal,
    /// `exec-and-forget <shell>`: run through `/bin/sh -c`, never waited on.
    Exec(String),
    ReloadConfig,
    Enable(Toggle),
    ListWindows {
        json: bool,
        all: bool,
    },
    ListWorkspaces {
        json: bool,
        all: bool,
    },
    ListMonitors {
        json: bool,
    },
    ListApps {
        json: bool,
    },
    ListModes {
        json: bool,
    },
    TriggerBinding {
        binding: String,
        mode: Option<String>,
    },
    Checkpoint(CheckpointCmd),
    /// Pin the which-key sheet for the current level.
    Help,
    /// Show the desk: every monitor's workspaces and their windows, on the
    /// sheet, until a key resolves it.
    Overview(Option<usize>),
    Quit,
    /// Replace the running daemon with the `tatamid` on disk, in place.
    HotSwap,
    /// Open `config.toml` in the configured editor, writing the commented
    /// template first when there is no file.
    EditConfig,
    /// Start the daemon at login (the launchd agent): on, off or toggle.
    StartAtLogin(Toggle),
    Ping,
    Version,
    Subscribe(Vec<String>),
    DebugSetFrame {
        handle: u64,
        rect: crate::geom::Rect,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ActionError {
    #[error("empty command")]
    Empty,
    #[error("unknown command `{0}`")]
    Unknown(String),
    #[error("`{cmd}`: {msg}")]
    Usage { cmd: String, msg: String },
}

fn usage(cmd: &str, msg: impl Into<String>) -> ActionError {
    ActionError::Usage {
        cmd: cmd.to_string(),
        msg: msg.into(),
    }
}

/// Pull the boolean flags we understand out of `rest`; anything else that
/// starts with `--` is an error so typos never silently become positionals.
struct Flags {
    wrap: bool,
    focus_follows: bool,
    json: bool,
    all: bool,
    mode: Option<String>,
    window_id: Option<u64>,
}

fn take_flags(cmd: &str, rest: &[&str]) -> Result<(Flags, Vec<String>), ActionError> {
    let mut f = Flags {
        wrap: false,
        focus_follows: false,
        json: false,
        all: false,
        mode: None,
        window_id: None,
    };
    let mut pos = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "--wrap-around" => f.wrap = true,
            "--focus-follows-window" => f.focus_follows = true,
            "--json" => f.json = true,
            "--all" => f.all = true,
            "--mode" => {
                i += 1;
                f.mode = Some(
                    rest.get(i)
                        .ok_or_else(|| usage(cmd, "--mode needs a value"))?
                        .to_string(),
                );
            }
            "--window-id" => {
                i += 1;
                let v = rest
                    .get(i)
                    .ok_or_else(|| usage(cmd, "--window-id needs a value"))?;
                f.window_id = Some(
                    v.parse()
                        .map_err(|_| usage(cmd, format!("bad window id `{v}`")))?,
                );
            }
            // AeroSpace flags we accept and ignore so configs port cleanly.
            "--fail-if-noop"
            | "--auto-back-and-forth"
            | "--ignore-floating"
            | "--fail-if-fullscreen" => {}
            s if s.starts_with("--") => return Err(usage(cmd, format!("unknown flag {s}"))),
            s => pos.push(s.to_string()),
        }
        i += 1;
    }
    Ok((f, pos))
}

fn one<'a>(cmd: &str, pos: &'a [String], what: &str) -> Result<&'a str, ActionError> {
    match pos {
        [x] => Ok(x.as_str()),
        [] => Err(usage(cmd, format!("missing {what}"))),
        _ => Err(usage(
            cmd,
            format!("expected one {what}, got {}", pos.len()),
        )),
    }
}

fn dir_of(cmd: &str, s: &str) -> Result<Dir, ActionError> {
    Dir::parse(s).ok_or_else(|| usage(cmd, format!("expected left|down|up|right, got `{s}`")))
}

fn ws_target(s: &str) -> WorkspaceTarget {
    match s {
        "next" => WorkspaceTarget::Next,
        "prev" => WorkspaceTarget::Prev,
        _ => WorkspaceTarget::Name(s.to_string()),
    }
}

fn toggle(cmd: &str, pos: &[String]) -> Result<Toggle, ActionError> {
    Ok(match pos {
        [] => Toggle::Toggle,
        [s] => match s.as_str() {
            "on" => Toggle::On,
            "off" => Toggle::Off,
            "toggle" => Toggle::Toggle,
            _ => return Err(usage(cmd, format!("expected on|off|toggle, got `{s}`"))),
        },
        _ => return Err(usage(cmd, "too many arguments")),
    })
}

impl Action {
    /// Parse one command from its words (`["focus", "left"]`).
    pub fn parse<S: AsRef<str>>(words: &[S]) -> Result<Action, ActionError> {
        let words: Vec<&str> = words.iter().map(|w| w.as_ref()).collect();
        let (cmd, rest) = words.split_first().ok_or(ActionError::Empty)?;
        let cmd = *cmd;
        // exec takes the rest of the line verbatim, flags and all.
        if cmd == "exec-and-forget" || cmd == "exec" {
            if rest.is_empty() {
                return Err(usage(cmd, "missing shell command"));
            }
            return Ok(Action::Exec(rest.join(" ")));
        }
        let (f, pos) = take_flags(cmd, rest)?;
        Ok(match cmd {
            "focus" if f.window_id.is_some() => Action::FocusWindow(f.window_id.unwrap_or(0)),
            "focus" => match one(cmd, &pos, "direction")? {
                "dfs-next" => Action::FocusDfs { next: true },
                "dfs-prev" => Action::FocusDfs { next: false },
                s => Action::Focus {
                    dir: dir_of(cmd, s)?,
                    wrap: f.wrap,
                },
            },
            "focus-back-and-forth" => Action::FocusBackAndForth,
            "move" => Action::Move {
                dir: dir_of(cmd, one(cmd, &pos, "direction")?)?,
            },
            "swap" => match one(cmd, &pos, "direction")? {
                "dfs-next" => Action::SwapDfs { next: true },
                "dfs-prev" => Action::SwapDfs { next: false },
                s => Action::Swap {
                    dir: dir_of(cmd, s)?,
                    wrap: f.wrap,
                },
            },
            "resize" => {
                let (dim, num) = match pos.as_slice() {
                    [dim, num] => (dim.as_str(), num.as_str()),
                    [num] => ("smart", num.as_str()),
                    _ => {
                        return Err(usage(
                            cmd,
                            "usage: resize [smart|smart-opposite|width|height] [+|-]<px>",
                        ));
                    }
                };
                let dim = match dim {
                    "smart" => ResizeDim::Smart,
                    "smart-opposite" => ResizeDim::SmartOpposite,
                    "width" => ResizeDim::Width,
                    "height" => ResizeDim::Height,
                    _ => return Err(usage(cmd, format!("unknown dimension `{dim}`"))),
                };
                let parse_num = |s: &str| {
                    s.parse::<f64>()
                        .map_err(|_| usage(cmd, format!("bad number `{s}`")))
                };
                let delta = if let Some(n) = num.strip_prefix('+') {
                    ResizeDelta::Add(parse_num(n)?)
                } else if let Some(n) = num.strip_prefix('-') {
                    ResizeDelta::Sub(parse_num(n)?)
                } else {
                    ResizeDelta::Set(parse_num(num)?)
                };
                Action::Resize { dim, delta }
            }
            "layout" => {
                if pos.is_empty() {
                    return Err(usage(cmd, "missing layout"));
                }
                let args = pos
                    .iter()
                    .map(|s| {
                        LayoutArg::parse(s)
                            .ok_or_else(|| usage(cmd, format!("unknown layout `{s}`")))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Action::Layout(args)
            }
            "fullscreen" => Action::Fullscreen(toggle(cmd, &pos)?),
            "workspace" => Action::Workspace {
                target: ws_target(one(cmd, &pos, "workspace")?),
                wrap: f.wrap,
            },
            "workspace-back-and-forth" => Action::WorkspaceBackAndForth,
            "move-node-to-workspace" => Action::MoveNodeToWorkspace {
                target: ws_target(one(cmd, &pos, "workspace")?),
                focus_follows: f.focus_follows,
                wrap: f.wrap,
            },
            "move-node-to-monitor" => Action::MoveNodeToMonitor {
                target: MonitorTarget::parse(one(cmd, &pos, "monitor")?),
                focus_follows: f.focus_follows,
                wrap: f.wrap,
            },
            "move-workspace-to-monitor" => Action::MoveWorkspaceToMonitor {
                target: MonitorTarget::parse(one(cmd, &pos, "monitor")?),
                wrap: f.wrap,
            },
            "summon-workspace" => Action::SummonWorkspace(one(cmd, &pos, "workspace")?.to_string()),
            "focus-monitor" => Action::FocusMonitor {
                target: MonitorTarget::parse(one(cmd, &pos, "monitor")?),
                wrap: f.wrap,
            },
            "flatten-workspace-tree" => Action::FlattenWorkspaceTree,
            "join-with" => Action::JoinWith(dir_of(cmd, one(cmd, &pos, "direction")?)?),
            "flip" => Action::Flip,
            "split" => Action::Split(match one(cmd, &pos, "orientation")? {
                "horizontal" => SplitArg::Horizontal,
                "vertical" => SplitArg::Vertical,
                "opposite" => SplitArg::Opposite,
                s => {
                    return Err(usage(
                        cmd,
                        format!("expected horizontal|vertical|opposite, got `{s}`"),
                    ));
                }
            }),
            "balance-sizes" => Action::BalanceSizes,
            "close" => Action::Close,
            "close-all-windows-but-current" => Action::CloseAllWindowsButCurrent,
            "mode" => match one(cmd, &pos, "mode name")? {
                "normal" => Action::Normal,
                m => Action::Mode(m.to_string()),
            },
            "normal" => Action::Normal,
            "reload-config" => Action::ReloadConfig,
            "enable" => Action::Enable(toggle(cmd, &pos)?),
            "list-windows" => Action::ListWindows {
                json: f.json,
                all: f.all,
            },
            "list-workspaces" => Action::ListWorkspaces {
                json: f.json,
                all: f.all,
            },
            "list-monitors" => Action::ListMonitors { json: f.json },
            "list-apps" => Action::ListApps { json: f.json },
            "list-modes" => Action::ListModes { json: f.json },
            "trigger-binding" => Action::TriggerBinding {
                binding: one(cmd, &pos, "binding")?.to_string(),
                mode: f.mode,
            },
            "checkpoint" => Action::Checkpoint(match pos.as_slice() {
                [] => CheckpointCmd::Save(None),
                [s] if s == "list" => CheckpointCmd::List,
                [s] if s == "save" => CheckpointCmd::Save(None),
                [s, n] if s == "save" => CheckpointCmd::Save(Some(n.clone())),
                [s, n] if s == "restore" => CheckpointCmd::Restore(n.clone()),
                [s, n] if s == "delete" => CheckpointCmd::Delete(n.clone()),
                _ => {
                    return Err(usage(
                        cmd,
                        "usage: checkpoint [save [name] | restore <name> | list | delete <name>]",
                    ));
                }
            }),
            "help" | "keys" => Action::Help,
            "overview" => Action::Overview(match pos.as_slice() {
                [] => None,
                [n] => Some(
                    n.parse()
                        .map_err(|_| usage(cmd, format!("bad monitor number `{n}`")))?,
                ),
                _ => return Err(usage(cmd, "usage: overview [monitor-number]")),
            }),
            "quit" => Action::Quit,
            "hot-swap" => Action::HotSwap,
            "edit-config" => Action::EditConfig,
            "start-at-login" => Action::StartAtLogin(toggle(cmd, &pos)?),
            "ping" => Action::Ping,
            "version" => Action::Version,
            "subscribe" => {
                if f.all {
                    Action::Subscribe(
                        crate::ipc::IpcEvent::TOPICS
                            .iter()
                            .map(|s| s.to_string())
                            .collect(),
                    )
                } else {
                    if pos.is_empty() {
                        return Err(usage(cmd, "usage: subscribe <topic>... | --all"));
                    }
                    for t in &pos {
                        if !crate::ipc::IpcEvent::TOPICS.contains(&t.as_str()) {
                            return Err(usage(cmd, format!("unknown topic `{t}`")));
                        }
                    }
                    Action::Subscribe(pos)
                }
            }
            "debug-set-frame" => {
                let nums = pos
                    .iter()
                    .map(|s| {
                        s.parse::<f64>()
                            .map_err(|_| usage(cmd, format!("bad number `{s}`")))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                match nums.as_slice() {
                    [id, x, y, w, h] => Action::DebugSetFrame {
                        handle: *id as u64,
                        rect: crate::geom::Rect::new(*x, *y, *w, *h),
                    },
                    _ => {
                        return Err(usage(
                            cmd,
                            "usage: debug-set-frame <window-id> <x> <y> <w> <h>",
                        ));
                    }
                }
            }
            other => return Err(ActionError::Unknown(other.to_string())),
        })
    }

    /// Parse a line, splitting `;`-chained commands: `"move-node-to-workspace 3; workspace 3"`.
    /// Words are whitespace-separated; single or double quotes group words
    /// (so `exec-and-forget open -a "Google Chrome"` survives).
    pub fn parse_chain(line: &str) -> Result<Vec<Action>, ActionError> {
        let mut out = Vec::new();
        for part in split_semicolons(line) {
            let words = shell_words(&part);
            if words.is_empty() {
                continue;
            }
            out.push(Action::parse(&words)?);
        }
        if out.is_empty() {
            return Err(ActionError::Empty);
        }
        Ok(out)
    }

    /// True for actions that do nothing but mutate key-mode state; the key
    /// dispatcher handles these itself without a round trip to the reactor.
    pub fn is_mode_change(&self) -> bool {
        matches!(self, Action::Mode(_) | Action::Normal | Action::Help)
    }
}

/// Split on `;` outside quotes.
fn split_semicolons(line: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
                cur.push(c);
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                cur.push(c);
            }
            (None, ';') => parts.push(std::mem::take(&mut cur)),
            (None, c) => cur.push(c),
        }
    }
    parts.push(cur);
    parts
}

/// Minimal shell-style word split: whitespace separates, quotes group,
/// no escapes. Enough for `exec-and-forget open -a "App Name"`.
fn shell_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Action {
        Action::parse(&shell_words(s)).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn core_commands() {
        assert_eq!(
            p("focus left"),
            Action::Focus {
                dir: Dir::Left,
                wrap: false
            }
        );
        assert_eq!(
            p("focus right --wrap-around"),
            Action::Focus {
                dir: Dir::Right,
                wrap: true
            }
        );
        assert_eq!(p("focus dfs-next"), Action::FocusDfs { next: true });
        assert_eq!(p("focus --window-id 4421"), Action::FocusWindow(4421));
        assert_eq!(p("move down"), Action::Move { dir: Dir::Down });
        assert_eq!(
            p("swap up"),
            Action::Swap {
                dir: Dir::Up,
                wrap: false
            }
        );
        assert_eq!(
            p("resize smart +50"),
            Action::Resize {
                dim: ResizeDim::Smart,
                delta: ResizeDelta::Add(50.0)
            }
        );
        assert_eq!(
            p("resize width -50"),
            Action::Resize {
                dim: ResizeDim::Width,
                delta: ResizeDelta::Sub(50.0)
            }
        );
        assert_eq!(
            p("resize height 600"),
            Action::Resize {
                dim: ResizeDim::Height,
                delta: ResizeDelta::Set(600.0)
            }
        );
        assert_eq!(
            p("resize +10"),
            Action::Resize {
                dim: ResizeDim::Smart,
                delta: ResizeDelta::Add(10.0)
            }
        );
        assert_eq!(
            p("layout tiles horizontal vertical"),
            Action::Layout(vec![
                LayoutArg::Tiles,
                LayoutArg::Horizontal,
                LayoutArg::Vertical
            ])
        );
        assert_eq!(
            p("layout h_accordion"),
            Action::Layout(vec![LayoutArg::Exact(Layout::HAccordion)])
        );
        assert_eq!(p("fullscreen"), Action::Fullscreen(Toggle::Toggle));
        assert_eq!(p("fullscreen on"), Action::Fullscreen(Toggle::On));
        assert_eq!(
            p("workspace 3"),
            Action::Workspace {
                target: WorkspaceTarget::Name("3".into()),
                wrap: false
            }
        );
        assert_eq!(
            p("workspace next --wrap-around"),
            Action::Workspace {
                target: WorkspaceTarget::Next,
                wrap: true
            }
        );
        assert_eq!(
            p("move-node-to-workspace 2 --focus-follows-window"),
            Action::MoveNodeToWorkspace {
                target: WorkspaceTarget::Name("2".into()),
                focus_follows: true,
                wrap: false
            }
        );
        assert_eq!(
            p("focus-monitor left"),
            Action::FocusMonitor {
                target: MonitorTarget::Dir(Dir::Left),
                wrap: false
            }
        );
        assert_eq!(
            p("move-workspace-to-monitor dell"),
            Action::MoveWorkspaceToMonitor {
                target: MonitorTarget::Pattern("dell".into()),
                wrap: false
            }
        );
        assert_eq!(p("split opposite"), Action::Split(SplitArg::Opposite));
        assert_eq!(p("flip"), Action::Flip);
        assert_eq!(p("overview"), Action::Overview(None));
        assert_eq!(p("overview 2"), Action::Overview(Some(2)));
        assert_eq!(p("mode resize"), Action::Mode("resize".into()));
        assert_eq!(p("mode normal"), Action::Normal);
        assert_eq!(p("normal"), Action::Normal);
        assert_eq!(p("enable toggle"), Action::Enable(Toggle::Toggle));
        assert_eq!(
            p("list-windows --json --all"),
            Action::ListWindows {
                json: true,
                all: true
            }
        );
        assert_eq!(
            p("trigger-binding alt-h --mode main"),
            Action::TriggerBinding {
                binding: "alt-h".into(),
                mode: Some("main".into())
            }
        );
        assert_eq!(
            p("checkpoint"),
            Action::Checkpoint(CheckpointCmd::Save(None))
        );
        assert_eq!(
            p("checkpoint save work"),
            Action::Checkpoint(CheckpointCmd::Save(Some("work".into())))
        );
        assert_eq!(
            p("checkpoint restore work"),
            Action::Checkpoint(CheckpointCmd::Restore("work".into()))
        );
        assert_eq!(
            p("subscribe workspace mode"),
            Action::Subscribe(vec!["workspace".into(), "mode".into()])
        );
        assert_eq!(
            p("debug-set-frame 42 0 0 800 600"),
            Action::DebugSetFrame {
                handle: 42,
                rect: crate::geom::Rect::new(0.0, 0.0, 800.0, 600.0)
            }
        );
    }

    #[test]
    fn exec_keeps_the_whole_line() {
        assert_eq!(
            p("exec-and-forget open -a 'Google Chrome'"),
            Action::Exec("open -a Google Chrome".into())
        );
        assert_eq!(
            Action::parse_chain("exec-and-forget open -a \"Google Chrome\"").unwrap(),
            vec![Action::Exec("open -a Google Chrome".into())]
        );
    }

    #[test]
    fn chains_split_on_semicolons_outside_quotes() {
        let v = Action::parse_chain("move-node-to-workspace 3; workspace 3").unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(
            v[1],
            Action::Workspace {
                target: WorkspaceTarget::Name("3".into()),
                wrap: false
            }
        );
        let v = Action::parse_chain("exec-and-forget sh -c 'a; b'; workspace 1").unwrap();
        assert_eq!(v[0], Action::Exec("sh -c a; b".into()));
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn errors_are_specific() {
        assert_eq!(Action::parse::<&str>(&[]).unwrap_err(), ActionError::Empty);
        assert!(matches!(
            Action::parse(&["frobnicate"]).unwrap_err(),
            ActionError::Unknown(_)
        ));
        assert!(matches!(
            Action::parse(&["focus"]).unwrap_err(),
            ActionError::Usage { .. }
        ));
        assert!(matches!(
            Action::parse(&["focus", "sideways"]).unwrap_err(),
            ActionError::Usage { .. }
        ));
        assert!(matches!(
            Action::parse(&["focus", "left", "--bogus"]).unwrap_err(),
            ActionError::Usage { .. }
        ));
        assert!(matches!(
            Action::parse(&["subscribe", "nope"]).unwrap_err(),
            ActionError::Usage { .. }
        ));
        // AeroSpace-only flags are tolerated so configs port.
        assert_eq!(p("workspace 1 --auto-back-and-forth"), p("workspace 1"));
    }

    #[test]
    fn serde_round_trip() {
        let a = Action::MoveNodeToWorkspace {
            target: WorkspaceTarget::Name("x".into()),
            focus_follows: true,
            wrap: false,
        };
        let s = serde_json::to_string(&a).unwrap();
        assert_eq!(serde_json::from_str::<Action>(&s).unwrap(), a);
    }
}
