# tatami user guide

This is the day-to-day reference: task recipes, not a tour. If you're
brand new, read the README first. Everything here assumes the current
`main`; tatami has no numbered release yet.

## Contents

- [Installing and upgrading](#installing-and-upgrading)
- [Starting, stopping, and Accessibility](#starting-stopping-and-accessibility)
- [How tatami sees your desk](#how-tatami-sees-your-desk)
- [Keys](#keys)
- [Working with tiles](#working-with-tiles)
- [Workspaces and monitors](#workspaces-and-monitors)
- [The mouse](#the-mouse)
- [The key sheet](#the-key-sheet)
- [The menu bar item](#the-menu-bar-item)
- [Seeing what is where](#seeing-what-is-where)
- [Windows that won't tile](#windows-that-wont-tile)
- [Layouts that survive a restart](#layouts-that-survive-a-restart)
- [Configuration recipes](#configuration-recipes)
- [The CLI and scripting](#the-cli-and-scripting)
- [Diagnostics](#diagnostics)
- [Uninstalling](#uninstalling)

## Installing and upgrading

The repository is private; clone with a logged-in `gh`.

```sh
gh repo clone dhishumb/tatami && cd tatami
./scripts/install.sh                 # build, install, sign, start at login, run
```

What the installer does, in full mode:

- builds `tatamid` and `tatami` and copies them to `~/.local/bin/`
  (`PREFIX=` to change), warning if that directory is not on `PATH`;
- signs both with a local self-signed identity (`tatami-codesign`) so the
  Accessibility grant survives upgrades. The first run creates the
  identity and macOS asks for your password once
  ([CODE_SIGNING.md](CODE_SIGNING.md)); `TATAMI_SKIP_CODESIGN=1` skips;
- writes `~/.config/tatami/config.toml` if there isn't one (never
  touched afterwards);
- installs a launchd agent so `tatamid` starts at login and is
  restarted if it dies (`TATAMI_NO_SERVICE=1` skips), and starts it.

Upgrading:

```sh
git pull && ./scripts/install.sh --upgrade
```

`--upgrade` installs and signs the binaries and, if the daemon's binary
changed, **hot-swaps** the running daemon: it execs the new `tatamid`
over itself, same pid and same socket, with the layout handed across.
No window moves, hidden workspaces stay hidden, and a `tatami` command
sent meanwhile waits in the socket's backlog rather than failing. The
installer prints the new version once the new image is serving.
`tatami hot-swap`, `alt+s u` or the menu bar item do the same at any
time; `kill -USR1 <pid>` too. A daemon from before hot swap, or a swap
that fails (the old daemon keeps running in that case and says why in
the log), gets the cold restart instead: `tatami quit` saves the layout
and puts every window back on screen, launchd (or the script) starts
the new build, and the new build restores the layout. Nothing else is
touched. `TATAMI_NO_RELAUNCH=1` leaves a running daemon alone;
`SKIP_BUILD=1` reuses `target/release/`.

Quit Rectangle, yabai, AeroSpace or Amethyst first. Two managers fight
over every window, and Rectangle's Alt shortcuts collide with tatami's.

## Starting, stopping, and Accessibility

```sh
tatamid                   # run in the foreground (logs to the terminal)
tatami service status     # is the launchd agent installed and loaded?
tatami service restart    # restart under launchd
tatami hot-swap           # replace the daemon with the installed build, in place
tatami quit               # stop the daemon cleanly (layout saved, windows un-parked)
tatami ping               # is it alive? also reports Accessibility trust
```

tatami needs the Accessibility permission: System Settings → Privacy &
Security → Accessibility → `tatamid` on. The first start asks. Until it
is granted, the daemon runs but manages nothing and the menu bar item
says so; once granted it picks everything up without a restart. If you
start `tatamid` from a terminal, the grant is attributed to the
terminal app instead; that is fine for trying things, but use the
launchd agent for daily use.

The log is `~/.local/state/tatami/log/tatamid.log` under launchd
(`RUST_LOG=debug` in the agent's environment for more; the plist is
`~/Library/LaunchAgents/com.vgupta.tatami.plist`).

## How tatami sees your desk

Each monitor shows one **workspace**; each workspace is a **tree** of
tiles. A container in the tree lays its children out as side-by-side
tiles (`h_tiles`), stacked tiles (`v_tiles`) or an accordion (all
children on top of each other, with a sliver of each neighbour showing).
Windows share a container's space by weight, equal unless you resize.

Every tatami action applies to the **monitor under the mouse pointer**.
macOS's own idea of which app is focused is unreliable on a multi-monitor
desk (see [Diagnostics](#diagnostics) for why), so "where the pointer is"
is the rule: press a chord with the pointer over a monitor and that
monitor's focused tile is the one that moves, resizes or flips, and the
key sheet opens on that monitor.

Floating windows sit outside the tree, above it, where their app put
them. Fixed-size apps float automatically.

## Keys

Bindings come from a preset plus your overrides in
`~/.config/tatami/config.toml`. `tatami keymap` prints the active preset;
hold Alt for 300 ms and the key sheet lists the live bindings.

The default `tatami` preset has plyterm's shape: Alt chords for the
daily set, four **group** chords that open a mode (`alt+w` workspaces,
`alt+m` move, `alt+r` resize, `alt+s` service) followed by one letter,
and nothing is three keys. Modes marked *sticky* keep taking letters
until `Esc` or `Enter`; the others leave after one.

| chord | action |
|---|---|
| `alt+h` `alt+j` `alt+k` `alt+l` | focus left / down / up / right; past the edge, the next monitor |
| `alt+n` / `alt+p` | next / previous tile in tree order, wrapping |
| `alt+shift+h/j/k/l`, `alt+shift+←↑↓→` | move the tile; past the edge, onto the next monitor |
| `alt+←` `alt+→` / `alt+↑` `alt+↓` | width −50 / +50 px, height −50 / +50 px |
| `alt+e` | flip: this tile and its neighbour, side by side ↔ stacked (again to undo) |
| `alt+shift+e` | flip the whole container: horizontal ↔ vertical |
| `alt+v` / `alt+b` | split: the next window opens below / beside this one |
| `alt+f` | fullscreen toggle (fills the monitor; the others stay laid out behind) |
| `alt+t` | floating ↔ tiling |
| `alt+a` | accordion: horizontal ↔ vertical; `alt+shift+e` goes back to tiles |
| `alt+1` … `alt+9` | workspace N on the monitor under the pointer |
| `alt+shift+1` … `9` | move the tile to workspace N and follow it |
| `alt+tab` | the previous workspace |
| `alt+/` | pin the key sheet |
| `alt+o` | the desk on the main monitor as an outline: `↑`/`↓` move, `→` expands a monitor, `←` collapses it, `Enter` or a letter focuses a window, `/` searches; `Esc` or `alt+o` closes |
| `alt+w` + | **workspaces**: `h`/`l` previous/next, `n`/`p` next/previous monitor, `m` move this workspace to the next monitor, `1-9` |
| `alt+m` + (sticky) | **move**: `hjkl` move, `HJKL` move onto the monitor in that direction, `n` swap with the next tile, `1-9` move to workspace N |
| `alt+r` + (sticky) | **resize**: `h`/`l` width, `j`/`k` height, `-`/`+` smart (along the container's axis), `=` balance |
| `alt+s` + | **service**: `r` reload config, `f` flatten the workspace, `b` balance, `c` save a checkpoint, `e` tiling on/off, `u` hot-swap to the installed build, `q` quit |

Unbound Alt chords pass through to the app. Inside a mode every key is
captured until you leave. A chord that cannot act (nothing to the left,
not tiled) shows its reason in the menu bar for a couple of seconds.

Keys are physical: `alt+shift+1` is its own chord, and `+` in the resize
mode means shift and `=`.

## Working with tiles

**A new window gets its own workspace** on the monitor it opens on: the
lowest of `1`…`9` with nothing in it, which tatami switches to, so the
window fills the screen. Ten VS Code windows land on `1`…`9` and the
tenth tiles into the visible one. Tiling is then a deliberate act: move a
window into another's workspace (`alt+shift+8` moves the focused one to
8 and follows it; `alt+m` `8` moves it without following) and the two
share that screen. An empty workspace is forgotten when you leave it, so
its number is free again. `alt+o` is the map of it all.

`new_window = "tile"` under `[workspaces]` restores i3's habit: a new
window opens beside the focused tile in the visible workspace. In that
mode `alt+v` first makes it open below instead (`alt+b`: beside, when
the container is a column); the split takes effect for the next window.
On an existing neighbour use `alt+e`.

**Flip** (`alt+e`) is the one-key split. In a row of four, it takes the
focused tile and its right-hand neighbour (left, if none) and stacks the
two in one column; the other tiles keep theirs. Press it again on either
of the pair and they go back side by side at the row's usual share.
`alt+shift+e` flips the whole container instead.

**Move** (`alt+shift+h/j/k/l`) swaps with the neighbour in that
direction. At the edge of a nested container the tile hops out one
level; at the edge of the monitor it moves onto the next monitor's
visible workspace. `alt+m` then `n` swaps with the next tile in tree
order when direction is awkward.

**Resize** nudges the focused tile's share by 50 px. `alt+r` enters the
sticky mode for several steps; `=` restores equal shares, `alt+s f`
flattens nested containers into one row.

**Fullscreen** (`alt+f`) is tatami's, not macOS's: the window fills the
monitor's visible area, the rest of the workspace stays laid out behind
it, and `alt+f` again puts it back. Focus keys still work while a window
is fullscreen; the newly focused tile is raised over it. macOS's own
green-button fullscreen takes the window out of tatami's hands until it
leaves that mode.

**Float** (`alt+t`) takes a window out of the tree where it is; `alt+t`
again tiles it back beside the focused tile.

## Workspaces and monitors

Each monitor has its own workspaces `1`…`9`. `alt+3` with the pointer over
the left monitor shows that monitor's 3; the right monitor can show its
own 3 at the same time. A workspace is created when first used and
forgotten when it is empty and not visible, unless listed under
`persistent` in the config.

Hidden workspaces keep their windows **parked** one point inside their
own monitor's edge, in a corner no neighbouring monitor touches, so they
never show on another display. Clicking such a window in the Dock or
Cmd-Tabbing to it switches to its workspace. Quitting tatami, a
SIGTERM, or the next start after a crash puts every parked window back.

`alt+w` `n`/`p` moves focus to the next/previous monitor (left-to-right
order), and `alt+h/j/k/l` past the edge of a monitor crosses to the one
in that direction. Either way the pointer comes along, so the next chord
acts on the monitor you just reached (`mouse_follows_focus = false`
under `[general]` turns that off, at which point the keys and the pointer
rule disagree). `alt+w` `m` carries the whole visible workspace to the
next monitor, merging it with a same-named workspace there. `alt+m`
`H`…`L` move one tile onto the monitor in that direction.

Unplug a monitor and its workspaces fold onto the main monitor as hidden
workspaces; plug it back in and they return. Monitors are identified by
their hardware UUID, so the arrangement survives a reboot.

AeroSpace's model, one shared pool of workspaces each pinned to a
monitor, is `model = "shared"` under `[workspaces]`; `summon-workspace`
and `monitor-force-assignment` work as there. Changing the model needs a
restart.

## The mouse

- **Drag a tile onto another monitor** and it moves to that monitor's
  visible workspace when you let go.
- **Drag a tile over another tile** on the same monitor and the two swap.
- Drag it anywhere else, or resize it by its edges, and it snaps back.
- Floating windows go wherever you put them.

tatami decides 200 ms after the last move event, so a drag is never
fought mid-way.

## The key sheet

Hold Alt alone and after 300 ms a sheet opens along the bottom of the
monitor under the pointer listing every direct chord: keys in columns,
prefixes spelled `+workspaces`. Press a group chord and pause, and it
lists that group. A chord typed at speed never sees it; once open it
follows you deeper and closes when the chord resolves, Alt is released,
or `Esc`. `alt+/` pins the current level until a key resolves it.
`which_key_delay_ms` under `[general]` changes the delay (0 opens at
once).

## The menu bar item

The grid icon shows the visible workspace of the monitor under the
pointer and the active mode (`[2] RESIZE`). Its menu lists every
monitor's workspaces (the visible one checked, the focused monitor
marked) to switch with a click, and the service actions: show keys,
balance, flatten, tiling on/off, save checkpoint, reload config, edit
config, hot-swap to the installed build, quit. `enable toggle` (`alt+s e`) stops managing without
quitting: every window is un-parked and left alone until you turn it
back on.

## Seeing what is where

`alt+o` opens the desk on the main display (`overview_monitor` under
`[general]` changes that: `pointer`, `focused`, a monitor number counted
left to right, or part of a monitor's name), keyboard first. It is an
outline: one header per monitor, left to right, with its window count
(the monitor you're on marked `◆`); the monitor you're on opens expanded
(`▾`), the others collapsed (`▸`). Under an expanded monitor, its
workspaces (visible one starred) and one line per window with a letter
(`~` marks a floating one).

- `↑`/`↓` move the highlight over headers and windows; it starts on the
  window you're in.
- `→` expands the highlighted monitor, `←` collapses it (from one of its
  windows too, landing on the header); `Enter` on a header toggles it.
  `1`…`9` expand monitor N and jump to it.
- `Enter` on a window focuses it; **or press a window's letter**. Either
  way the window is focused wherever it is: its workspace is switched to
  and the pointer moves to its monitor.
- `/` (or space) starts a search over every monitor, collapsed or not:
  type, and the list narrows to windows whose app, title, workspace or
  monitor contains the text; `↑`/`↓` and `Enter` work the same, Backspace
  edits (past an empty search you're back to the outline).
- `Esc` or `alt+o` closes; any other chord closes it and runs as usual.

Lines and headers can also be clicked, but nothing needs the mouse. This is
also the app switcher for windows Cmd-tab cannot reach (plyterm, after
it re-execs itself, is absent from macOS's app list). The menu bar item's
menu lists the same workspaces with window counts.

From a shell:

```sh
tatami list-monitors              # index, uuid, name, frame, which is focused
tatami list-workspaces --all      # per monitor: name, window count, visible, focused
tatami list-windows --all         # every window: handle, app, title, workspace, frame
tatami list-windows --all --json  # the same with monitor, kind, focus
```

## Windows that won't tile

Some windows cannot fill a tile and tatami floats them:

- apps whose window refuses to be resized at all (utility panels);
- System Settings, Calculator, Archive Utility, Installer and Digital
  Color Meter, by built-in rules (your own `[[rules]]` are checked
  first and win);
- dialogs and sheets, which are never adopted unless a rule names them.

Apps with a **minimum width** (many Electron apps, Finder, browsers at
narrow widths) do tile, but never below that width. Your resizes always
win: a tile gets at least its share, and a neighbour that cannot shrink
keeps its minimum and is overlapped instead, with the focused tile on
top and the row still ending at the screen edge. Four such apps side by
side on a 1920 px monitor overlap from the start: pair two of them with
`alt+e`, or move one to another workspace.

A tile whose app ignores or refuses a size is logged
(`window refuses its frame`) and remembered for that dimension.

## Layouts that survive a restart

The tree of every workspace on every monitor is saved to
`~/.local/state/tatami/state.json` a second after the last change, every
five seconds when something changed, and on quit. It is plain JSON,
readable and hand-editable.

On start, tatami maps the saved monitors to the live ones by UUID, puts
each window it recognises (same app, then by title) back into its slot,
and relaunches apps that are missing, slotting their windows in as they
appear. Unmatched windows take a normal slot; empty slots are dropped
after `restore.timeout_s`. `restore.launch_apps = false` turns off the
relaunching; `restore.ignore` lists bundle ids never to relaunch.

Named layouts:

```sh
tatami checkpoint save work       # ~/.local/state/tatami/checkpoints/work.json
tatami checkpoint list
tatami checkpoint restore work    # re-slot live windows, launch what is missing
tatami checkpoint delete work
```

`alt+s c` saves one named by the time.

## Configuration recipes

`~/.config/tatami/config.toml`, hot-reloaded on save (also `alt+s r` or
`tatami reload-config`). Every key is optional.

Change the gaps and the default layout:

```toml
[gaps]
inner = 12
outer = { top = 12, bottom = 12, left = 12, right = 12 }

[layout]
default = "v_tiles"
```

Float an app, send another to a workspace, leave a third alone:

```toml
[[rules]]
app_id = "com.apple.MobileSMS"
float = true

[[rules]]
app_id = "com.tinyspeck.slackmacgap"
workspace = "9"

[[rules]]
app_name_regex = "zoom.us"      # case-insensitive substring; ^ and $ anchor
ignore = true
```

Rebind a chord, add one, remove one, add to a mode:

```toml
[keys]
preset = "tatami"
"alt+enter" = "exec-and-forget open -a Ghostty"
"alt+shift+q" = "close"
"alt+a" = []                    # unbind

[keys.modes.resize]
"x" = ["balance-sizes", "normal"]   # one-shot inside the sticky mode
```

A whole preset of your own: `tatami keymap > ~/.config/tatami/keymaps/mine.toml`,
edit it, set `preset = "mine"`. A file can start from another with
`extends = "tatami"`. Commands are AeroSpace's names
(`focus left`, `move-node-to-workspace 3`, `layout accordion`), chained
with `;` or as a list; a list ending in `"normal"` leaves the mode.

Use Ctrl+Alt as the leader instead of Alt, keep the pointer where it is
when keys cross monitors, and open the overview on the big external
display rather than the laptop's (which macOS calls the main one while
the lid is open):

```toml
[general]
leader = "ctrl+alt"
mouse_follows_focus = false
overview_monitor = "CQ32G4"     # or "pointer", "focused", or a number (left to right)
```

Workspace names of your own and ones that are never forgotten:

```toml
[workspaces]
names = ["web", "code", "chat", "4", "5"]
persistent = ["chat"]
new_window = "tile"          # default "own-workspace": each new window on its own
```

## The CLI and scripting

Any command you can bind, you can run:

```sh
tatami focus left
tatami workspace 3
tatami move-node-to-monitor right --focus-follows-window
tatami layout tiles horizontal vertical
tatami list-windows --all --json
tatami list-workspaces --all
tatami list-monitors
tatami list-modes
```

Events as JSON lines, for a status bar or a script:

```sh
tatami subscribe workspace focus mode     # or --all
# {"event":"workspace_changed","monitor":"<uuid>","workspace":"3","prev":"1"}
```

The daemon listens on a Unix socket (`$XDG_RUNTIME_DIR/tatami/sock`, else
`~/.tatami/run/sock`; `tatami socket-path` prints it). Frames are a
4-byte little-endian length and a JSON body:
`{"id":1,"args":["focus","left"],"protocol":1}` →
`{"id":1,"ok":true,"stdout":"..."}`. A client built against another
protocol version gets an error naming both; the daemon never restarts
itself over it.

## Diagnostics

```sh
tatami ping                 # alive, trusted, window and workspace counts
tatami doctor               # socket, process, other window managers, Spaces setting
tatami service status
tail -f ~/.local/state/tatami/log/tatamid.log
cargo run -p tatami-platform --example probe -- windows     # what Accessibility reports, per app
```

- **Keys do nothing at all.** `tatami ping` says `trusted: false`: grant
  Accessibility and `tatami service restart`. Nothing in the log at
  all: the daemon isn't running; `tatami service status`.
- **A key does nothing in one place.** Watch the menu bar: a failed
  chord shows its reason there for 2.5 s (`no window to the left`, `the
  focused window is not tiled`).
- **The sheet or a chord acts on the wrong monitor.** It acts on the
  monitor under the pointer, by design. macOS cannot be trusted about
  focus here: it keeps a stale record (pid −1) for an app that re-exec'd
  itself, such as plyterm, lists utility panels as frontmost, and its
  system-wide focused-application query fails outright on some desks.
  tatami finds apps through the window server and listens to each app's
  own activation notification instead.
- **A window is tiled that shouldn't be, or the row won't split
  evenly.** `tatami list-windows --all --json` shows each window's
  `kind`; add a `[[rules]]` entry with `float = true`. A row with a
  fixed-width app in it can never be equal.
- **Windows are off-screen after a crash.** Start tatami: the next start
  un-parks strays. Without tatami, drag them back from Mission Control.
- **"another tatamid is listening"**: a second copy; `tatami quit` the
  first or `tatami service restart`.

## Uninstalling

```sh
tatami service uninstall
rm ~/.local/bin/{tatamid,tatami}
rm -rf ~/.config/tatami ~/.local/state/tatami    # config, layouts, checkpoints, log
security delete-identity -c tatami-codesign      # optional: the signing identity
```
