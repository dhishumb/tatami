# Changelog

All notable changes to tatami will be documented here. Format adapted from
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); tatami follows
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

The first cut, built and tuned on a four-monitor desk over 2026-10-10 and
2026-10-11. Nothing is tagged yet; install from `main` with
`./scripts/install.sh`.

### Added

- **Start at login is a switch.** The installer turned it on with no
  way to turn it off short of `tatami service uninstall`, which also
  stops the daemon. "Start at login" in the menu bar item (checked when
  the launchd agent is installed) and `tatami service enable|disable`
  flip it without touching the running daemon; `tatami service
  start|stop` run it under launchd.
- **"Edit config…" always has a file to open.** The menu item ran
  `open -t` on `~/.config/tatami/config.toml` and, with no file there,
  TextEdit said so and nothing happened. `edit-config` (the menu,
  `tatami edit-config`) first writes the template when the file is
  missing, every option at its default with a comment, then opens it in
  TextEdit or in `editor` under `[general]` (`"code"`, `"zed"`).
  `tatami config-template` prints the same, and the installer seeds a
  missing config from it. `[general] start_at_login`, which did nothing,
  is gone.
- **The install checks the daemon came up.** After installing the agent
  the script waits for `tatami ping` and prints what it said (version,
  Accessibility, counts); without Accessibility it says so and opens the
  System Settings pane. `tatami ping` itself now answers in words
  (`pong: tatamid 0.1.0, Accessibility granted, 8 windows on 6
  workspaces`) instead of `pong`.
- **The sheet is a little see-through** (84% instead of 94%).
- **Upgrades hot-swap the daemon.** `scripts/install.sh --upgrade` used
  to quit the daemon (every hidden window came back on screen for a
  moment) and start the new build, which restored the layout. The daemon
  now execs the new `tatamid` over itself: same pid, same socket, the
  layout handed across in a manifest, no window moves and nothing for
  launchd to notice. `tatami hot-swap`, `alt+s u`, the menu bar item or
  `SIGUSR1` do it at any time; the installer falls back to the cold
  restart for a daemon from before this change, and once more the first
  time you install this one. A saved layout is now also restored without
  a first tiling pass, so a start no longer flashes the interim layout.
- **New windows get their own workspace.** A window opens on the lowest
  empty workspace of its monitor and fills it; the tenth tiles into the
  visible one. Tiling is a move into another window's workspace
  (`alt+shift+N`, `alt+m N`). `new_window = "tile"` under `[workspaces]`
  keeps i3's open-beside-the-focused-tile behaviour.
- **The overview searches your keys too.** Forgetting a chord meant
  `alt+/` and reading. In the overview's search (`alt+o` `/`), the text
  now also matches bindings by chord, command or mode, listed under
  `keys` as you would press them (`alt+s u  hot-swap`), and `Enter` on
  one runs it. Keys match on word starts, so `h` lists what the `h` key
  does and a stray letter inside a word stays quiet.
- **The overview is an outline.** Monitor tabs switched with `←`/`→`
  put the arrows at odds with each other; now `↑`/`↓` walk one list of
  monitor headers and windows, `→` expands the highlighted monitor, `←`
  collapses it (from a window too), and `Enter` focuses a window or
  toggles a header. The highlight starts on the window you're in, with
  that monitor expanded and the others folded; `1`-`9` expand monitor N.
  A search looks under every monitor, folded or not. The list is set
  without the `→` on every line, with a blank line between monitors and
  a fixed column for the highlight, and it may use half the screen.
- **A tiling window manager for macOS in the shape of AeroSpace**, using
  only the public Accessibility API: a daemon (`tatamid`) that owns every
  window on the main thread's run loop, and a thin CLI (`tatami`) over a
  Unix socket speaking length-prefixed JSON. Commands use AeroSpace's
  names, so its configs port.
- **Workspaces per monitor.** Each monitor has its own `1`…`9`; `alt+3`
  switches the monitor under the pointer, and two monitors can show a
  `2` at once. AeroSpace's shared pool is `model = "shared"`. Hidden
  workspaces park their windows one point inside their own monitor's
  edge, in a corner no neighbour touches; quitting, a signal, or the
  next start after a crash puts everything back.
- **plyterm-shaped keys with Alt.** One-press chords for the daily set,
  four group chords (`alt+w` workspaces, `alt+m` move, `alt+r` resize,
  `alt+s` service), sticky and one-shot modes, `[modes."*"]`, presets
  with `extends`, and the which-key sheet: hold Alt 300 ms and it lists
  the chords on the monitor under the pointer. `alt+n`/`alt+p` cycle the
  tiles; `alt+arrows` resize and `alt+shift+arrows` move, as in plyterm.
- **`flip` (`alt+e`)**, the one-key split: the focused tile and its
  neighbour become a stacked pair; again to undo, back at the row's
  usual share. `alt+shift+e` flips the whole container. `alt+v`/`alt+b`
  choose where the next window opens.
- **Layouts that survive a restart.** `state.json` is written a second
  after every change; on start, windows go back to their slots and
  missing apps are relaunched and slotted in as they appear. Named
  checkpoints: `tatami checkpoint save|restore|list|delete`.
- **A menu bar item** with each monitor's workspaces and the service
  actions, and a status flash naming why a chord did nothing.
- **`alt+o`, the desk overview and switcher:** the sheet, always on the
  main monitor, lists every monitor with its workspaces (visible one
  starred) and a lettered line per window. A letter, or `/` then typing
  with `↑`/`↓` and `Enter`, focuses that window wherever it is; lines
  and headers can be clicked too. Cmd-tab cannot reach an app macOS has
  lost track of (plyterm after a hot swap); this can. `Esc`, a chord, or
  `alt+o` again closes it. `focus --window-id <n>` and `overview <n>`
  are the commands behind the clicks. It opens on the main display by
  default; `overview_monitor` under `[general]` picks another (`pointer`,
  `focused`, a number, part of a name) for desks where the laptop lid
  makes the wrong display "main".
- **Mouse:** drag a tile onto another monitor to move it there, over
  another tile to swap, anywhere else to snap back.
- **Minimum sizes belong to the layout.** A window that refuses a width
  keeps it and the other tiles pay; when the minimums alone exceed the
  monitor the tiles overlap evenly rather than leaving the screen.
  Fixed-size windows and System Settings, Calculator, Archive Utility,
  Installer and Digital Color Meter float by built-in rules; a user's
  `[[rules]]` are checked first.
- **Installer** in plyterm's shape: `scripts/install.sh` builds, installs
  to `~/.local/bin`, signs with a one-time `tatami-codesign` identity so
  the Accessibility grant survives upgrades, seeds the config, installs
  the launchd agent, and on `--upgrade` restarts a changed daemon cleanly.

### Fixed

- **Focusing a window from the overview no longer flips workspaces on
  other monitors.** Raising a window activates its app, and macOS
  reports the app's previous key window as focused first. When that
  window was parked on a hidden workspace of another monitor (Chrome
  with a window per monitor), the daemon took the report as a Cmd-tab
  onto it and switched that workspace in, so an `alt+o` pick could
  change what two monitors showed and leave the wrong window focused.
  A focus report for a parked window of the app we just raised, within
  a second, is now ignored and the asked-for window raised again; a
  real Cmd-tab or Dock click a moment later still switches.
- **`tatami quit` quits.** launchd's `KeepAlive` brought the daemon
  straight back (every quit in the log was followed by a restore two
  hundred milliseconds later). The agent now restarts a crash only;
  `tatami service start` or the next login brings it back.
- **Going to another monitor by keyboard sticks.** `alt+w n` and
  `alt+h/l` across an edge moved focus, but the next chord snapped back
  to the monitor under the pointer. The pointer now follows keyboard
  focus across monitors (`[general] mouse_follows_focus`, default on).
- **A config written after the daemon started is hot-reloaded.** The
  watcher gave up when `~/.config/tatami/` did not exist yet; it now
  creates the directory and watches it from the start.
- **`tatami subscribe --all` works.** The daemon checked the raw topic
  list and refused `--all`; it now expands to every topic.
- **Saving a checkpoint from `alt+s c` or the menu works.** Only the CLI
  path reached the daemon's checkpoint code; keys and the menu now hand
  the request over.
- **A floating window left parked by a killed daemon comes back.** Tiled
  strays always did, through the layout; floating ones were adopted where
  they were, one point inside a corner, and stayed there.
- **A resize always wins over a neighbour's minimum.** Growing a pair
  beside Bruno and Claude stopped once those two reached their minimum
  widths. The weighted share is now a floor for every tile: a neighbour
  that cannot shrink keeps its minimum and is overlapped instead, the
  focused tile is raised on top, and the row still ends at the screen
  edge.
- **Resizing works in a crowded row.** When a row's minimum widths
  already exceed the monitor (four Electron-style apps on a 1920 px
  display), every tile was pinned to exactly its minimum and resize did
  nothing. A tile now gets the larger of its minimum and its weighted
  share, so growing it makes it wider and it overlaps its neighbours
  more; everything stays on screen.
- **Resizing no longer tops out.** Growing a tile took equally from its
  siblings and stopped once the smallest would fall below a tenth of the
  row, so a tile in a row of four could never pass about 70%. The share
  floor is now a sliver; the real floor is each window's own minimum
  size, which the layout enforces.

### Known limitations

- The key table is the ANSI US layout; other layouts map by position.
- Resizing a tile by dragging its edge snaps back (resize with the keys).
- macOS's own fullscreen (the green button) takes a window out of
  tatami's hands until it leaves that mode.
- Hot-plugging a monitor is handled but has not been exercised much.
- No `.app` bundle: the Dock never shows tatami; the menu bar item is
  the way in.
