# tatami architecture

How the code is organised, what each crate owns, and why the
non-obvious choices were made. `CLAUDE.md` has the rules; this is the map.

## Process model

```
┌─ tatamid: one per user (launchd agent com.vgupta.tatami) ─────────┐
│ main thread: NSApp run loop, Accessory policy (no Dock icon)      │
│   Rc<RefCell<Daemon>> → Reactor<MacPlatform>: all WM state        │
│   ▲ AX observers, NSWorkspace notes, event tap, menu, CF timer    │
│   ▲ CF source "waker" ◄─ mpsc<MainMsg> ◄─┬─ tatami-ipc, -conn     │
│                                          ├─ tatami-signals        │
│                                          └─ config watcher        │
└──────────────────────────────▲────────────────────────────────────┘
                               │ Unix socket: u32 LE length + JSON
               tatami <argv…>  ·  tatami subscribe <topics>
```

All state lives on the main thread. The other threads own nothing:
each sends a `MainMsg` (`Ipc`, `ReloadConfig`, `Shutdown`, `HotSwap`)
and calls the waker, which signals the CF source and wakes the run
loop. AppKit and AX stay on the thread they demand, with no lock
anywhere.

### Daemon upgrade (hot swap)

The daemon replaces its own code in place (`tatamid/src/main.rs`
`hot_swap`, manifest in `handoff.rs`): same pid, so launchd sees one
process; the Accessibility grant, keyed to the signing identity,
carries over; no window moves and hidden workspaces stay parked. Three
triggers share the path: `SIGUSR1` (no socket needed), the `hot-swap`
command (`tatami hot-swap`, `alt+s u`, the menu; it answers first and
swaps on the next run-loop pass so the reply gets out), and the
installer, which uses the command and falls back to a cold restart.

1. Preflight: `<current_exe> --version` must print `tatamid …` and a
   `handoff <format>` line this build hands off to. A truncated file or
   a build from before hot swap is refused before anything is touched.
2. `state.json` is written, and the manifest (`handoff` next to the
   socket, mode 0600, whole-file rename): format, version, the `enable`
   flag and the layout as a `SavedState`, so a disabled `restore` does
   not lose it.
3. `exec` of the same path with `--adopt <manifest>`, the listening
   socket as standard input (`Stdio::from(OwnedFd)`: the dup drops
   close-on-exec, every other descriptor closes). All safe std:
   `CommandExt::exec`, `UnixListener::from(OwnedFd)`. The socket keeps
   listening across the exec, so a `tatami` sent meanwhile waits in the
   backlog instead of being refused.
4. The new image reads and deletes the manifest, adopts the listener
   from standard input after checking it is bound to the socket path,
   runs the ordinary start-up with the manifest's layout in place of
   `state.json` (the restore matcher finds every window by pid and
   window id), re-creates the AX observers, event tap and menu bar item
   (the old ones died with the old image's window-server connection),
   and writes `ok <version>` to `swap-status`.

If anything fails before the exec returns, the manifest is removed,
`swap-status` says `failed <why>` and the old image keeps serving. The
gap: signal handlers reset across `exec`, so a second signal arriving
before the new image installs its own takes the default action; the
installer sends one.

`tatami` is a thin client: argv goes to the daemon as one `Request`,
untouched, and is parsed by the same `Action` parser the keymap uses,
so every command is bindable. Only `help`, `--version`, `socket-path`,
`keymap(s)`, `service` and `doctor` run locally. `subscribe` keeps the
connection open and prints one JSON line per event.

## Crates

```
tatami-protocol            tatami-keymap               no internal deps
tatami-state  (protocol)   tatami-config (protocol, keymap)
tatami-core   (protocol, keymap, config, state)
tatami-platform (core, protocol)
tatamid (all of the above)    tatami (protocol, config)
```

- **protocol**: geometry, `Action` (the one command vocabulary, in
  AeroSpace's names), `Request`/`Response`/`IpcEvent`, the frame codec,
  the socket path. Add a command here first. Never touches macOS.
- **keymap**: chords, the keycode table, `Keymap<A>`, `KeyState`, the
  `WhichKey` clock. Generic over the action type and dependency-free,
  so every rule is tested with string actions and a fake clock.
- **config**: `config.toml` and the presets (`keymaps/tatami.toml` is
  embedded); builds `Keymap<Action>` once. A bad binding is a warning
  and is skipped; a TOML error keeps the config already loaded.
- **state**: the `state.json` schema, atomic save/load, checkpoint
  paths and the window matcher. Knows files, not trees.
- **core**: tree, layout, workspace model, parking, the `Platform`
  trait and the `Reactor`. Pure: no macOS types, tested against
  `fake::FakePlatform` (`--features fake`). Never depends on platform.
- **platform**: `MacPlatform`. All framework FFI and nearly all
  `unsafe`, one `// SAFETY:` per block. The rest of the `unsafe` is
  libc: `geteuid`/`getpeereid` in `tatamid/src/ipc.rs`, `getuid` in
  `tatami/src/service.rs`.
- **tatamid**: wiring: run loop, `step()`, autosave, IPC, signals,
  config watcher, checkpoints. **tatami**: the CLI, `service`, `doctor`.

## The reactor

`Reactor<P: Platform>` (`core/src/reactor.rs`, with `commands.rs` and
`restore.rs`) owns the platform and every piece of WM state. Time comes
in through every entry point as `now`; nothing inside reads the clock
to decide, so tests are deterministic. Entry points:

- `on_platform_event`: apps, windows, screens, trust, and `Command`
  (a menu pick, run like a CLI request).
- `on_key` + `drain` (below); `handle_request` (parse argv,
  `follow_mouse()`, run one `Action`).
- `on_timer` / `next_due`: which-key clock, frame verifies, restore
  deadlines, the 3 s window-server scan for unannounced apps, drag
  settle, flash expiry; `next_due` is the earliest.
- `reload_config`, `snapshot` / `restore`, `shutdown`, `set_enabled`.

Each returns `Outcome`s: `Dirty` (persist, debounced), `Broadcast`
(an `IpcEvent` for subscribers), `Exec` (`exec-and-forget`, run via
`/bin/sh -c`), `Exit`, `ReloadConfig`. Overlay, status text, menu and
the `ModeChanged` event are applied once per call in `finish()`, which
also floats any window whose app errored on a frame write.

The daemon's `step()` is the door: one `try_borrow_mut` of the
`RefCell<Daemon>` (a re-entry is skipped with a warning), the closure,
the outcomes, any platform events queued meanwhile, autosave, then the
single timer re-armed from `next_due`. Tap install (once trusted),
reload and shutdown need the `Rc`, so they run after the borrow drops.
Platform callbacks push onto a `VecDeque` and step only if the daemon
is free; otherwise the running step picks the event up. Keys:

```
CGEventTap callback (main thread: return fast, never call AX)
  └─ sink: try_borrow_mut → Reactor::on_key → KeyState::on_input
       ├─ queue actions, note overlay change → Swallow | Pass
       └─ MainLoop::perform(step(drain))       next pass of the loop
drain: follow_mouse() → run_one(action) each → finish()
```

`on_key` is the one call outside `step`; if the daemon is busy the key
passes to the app rather than vanish. `KeyState` applies `mode`,
`normal` and `help` itself; they are filtered out of the queue.

## Windows, the tree and layout

**Adoption** (`classify`). Role must be `AXWindow`. Rules come from
`Config::rules_in_effect()`, the user's `[[rules]]` then the built-ins
(float System Settings, Calculator, Archive Utility, Installer, Digital
Color Meter); the first match wins, so a user rule replaces the
built-in. `ignore` drops the window. Subrole must be `AXStandardWindow`
(or `AXDialog` if a rule matched). Native fullscreen is left alone
until it exits; a float rule or a non-settable `AXSize` floats; the
rest tile, after the focused leaf (at start-up, left to right). A
rule's `workspace` creates and targets that workspace.

**Tree** (`core/src/tree.rs`). One `SlotMap` arena for all workspace
roots, so moving a window between workspaces is a re-link. Containers
carry a `Layout` (tiles or accordion, H or V), children with `f32`
weights, and an `mru` child (which accordion child is raised, where
`move` descends). Leaves are `Window(id)` or `Pending(spec)`, a restore
placeholder that takes space but emits no placement. `normalize`
drops empty containers (never the root), flattens single-child ones,
splices a child with its parent's layout into the parent while keeping
shares, and lets the root adopt an only container child so its id stays
stable. A `sticky` container (fresh `split`) is exempt until its second
child arrives.

**Layout** (`layout::compute(tree, root, &Params, &mut out)`). One
pass into the caller's reused vector. `Params` holds the visible area,
gaps, focused leaf, accordion padding and `min: MinOf` (each window's
known minimum size). Tiles share their length by weight, water-filling:
a child that would fall below its minimum is clamped there and the rest
share what is left, until stable; whole points, the last unclamped
child absorbing the rounding. If the minimums alone exceed the space,
each child keeps its minimum and they overlap evenly, first at the left
edge, last at the right, so nothing leaves the monitor. Minimums never
touch weights. Accordions offset by `accordion_padding`, stack by `z`.

**Frame transactions** (`request_frame`, `verify_frames`). A request
within 1 pt of the current or pending frame is skipped. Every write is
`SetOrder::PosSizePos`, read back at 40, 90 and 250 ms and rewritten on
a mismatch; a second answer with an unchanged size at the right
position stops early. On giving up, `stubborn` records only the refused
dimension (the other stays free to shrink) and the workspace is laid
out again with that minimum. Stubborn is for life and skips verifies.
A frame event matching `expected` is our own echo. A write that returns
an AX *error* puts the window in `unmanageable`; `finish()` floats it.

**Commands** (`reactor/commands.rs`):

- `move dir`: swap with a sibling leaf; enter a sibling container at
  its near end; at the parent's edge hop out beside the nearest
  ancestor along that axis; at the root edge wrap the root; past that,
  go to the monitor in that direction, focus following.
- `join-with dir`: the nearest ancestor-or-self with a sibling in `dir`
  pairs with it in a new container of the opposite orientation.
- `flip` (`alt+e`): in a row, `join-with` the neighbour (right, else
  left; down, else up); on a pair, turn it. A pair turned back along
  its row's axis gets twice the row's average weight before it merges,
  so two flips are a round trip.
- `split`: wrap the leaf in a sticky container so the next window nests;
  splitting along the parent's own axis is refused.
- `resize`: the nearest ancestor-or-self whose parent tiles along the
  axis with siblings grows by the pixel delta converted to weight; the
  siblings pay evenly; nobody drops below 10% of the parent.

Our `fullscreen` keeps the leaf and fills the visible area inside the
outer gaps. A tile dragged by hand is left alone until 200 ms after its
last move event (`settle_drag`): dropped on another monitor it moves to
that monitor's visible workspace; over a sibling tile the two swap;
otherwise it snaps back. Hand resizes snap back too.

## Focus

macOS cannot be trusted about focus. `NSWorkspace` keeps a stale pid -1
record for an app that re-exec'd itself (plyterm's hot swap) and lists
the live pid nowhere; the system-wide `AXFocusedApplication` can fail;
the window server's front window can be a floating utility window
(Google Drive). So:

- `running_apps` adds every owner of a layer-0 on-screen window from
  `CGWindowListCopyWindowInfo`, with the window server's pid.
- Each app's `AXApplicationActivated` is observed. An activation the
  reactor cannot pin to a known window rescans for new apps and makes
  the pointer's monitor the focused one.
- `focused_window()` falls back: AX focused app, the app whose own
  `AXFrontmost` is set, the window server's front window, then
  `frontmostApplication` / `menuBarOwningApplication`.
- `follow_mouse()` runs before every chord batch and CLI request: the
  monitor under the pointer becomes the focused monitor, and if the
  focused window is elsewhere, that monitor's last-focused (or first)
  window takes over without a raise. The sheet opens there too.
- `default_focus()` gives direction keys a start when nothing is focused.

A `WindowFocused` for an unknown handle waits in `pending_focus` until
adoption; echoes of our own accordion raises (`raise_echo`) are
dropped; focus reaching a parked window (Cmd-Tab, Dock) switches to its
workspace.

## Workspaces and monitors

`WorkspaceModel` tracks names, placement and visibility; the reactor
owns the `Workspace` structs and acts on the returned `Switch` (hide,
show, focus monitor, merge).

- `PerMonitor` (default): each monitor has its own namespace, so `1`
  on the left and `1` on the right are different and both visible.
  `workspace 3` means the focused monitor's 3.
- `Shared` (AeroSpace): one namespace. Switching to a workspace visible
  on another monitor focuses that monitor instead of stealing it; a
  hidden one comes to the focused monitor unless
  `monitor-force-assignment` pins it. `summon-workspace` is shared-only.

Every connected monitor shows a workspace; one that gives its own away
gets a hidden one living there, else the first free configured name,
else the lowest free number. Empty, hidden, non-`persistent` workspaces
are garbage-collected. A model change needs a restart, and a state file
from the other model is not restored.

**Parking** (`core/src/parking.rs`). macOS refuses fully off-screen
moves, so a hidden workspace's windows keep their size and overlap
their own monitor by 1×1 pt in a corner. `parking_corner` probes BR,
BL, TR, TL with a square as large as the monitor (at least 2000 pt) and
takes the first that touches no other display, else the least
overlapping. A floating window's frame is kept in `float_frame` and
restored, refitted, when shown. `shutdown` and `enable off` un-park
everything. After a `kill -9`, the next start adopts each sliver
(`monitor_for_rect` falls back to the display it touches) and the first
layout pulls tiled strays back; a floating stray is not moved.

**Identity and hotplug.** A monitor is its UUID
(`CGDisplayCreateUUIDFromDisplayID`); `MonitorId`s live only while it
is connected; order is left to right by `frame.x`. `on_screens_changed`
ignores an empty screen list (display sleep), refreshes matched
monitors, gives new ones a workspace, moves a removed monitor's
workspaces hidden to the leftmost remaining one (per-monitor name
clashes merge their windows), recomputes parking corners and forced
assignments, relays out everything, broadcasts `monitors` and marks
the state dirty. Re-attaching a display does not bring its old
workspaces back; a checkpoint restore does.

## Keys

plyterm's shape: the chord is the prefix. Outside a mode only chords
holding the leader (`general.leader`, Alt) are looked up; plain typing
takes a fast path that touches neither the map nor the allocator.

- **Direct chords** run at once. Unbound leader chords pass to the app.
- **Modes** (`alt+w` workspaces, `alt+m` move, `alt+r` resize,
  `alt+s` service): every key is ours until `normal`. A binding list
  ending in `normal` is one-shot; anything else is sticky. Bindings
  match with or without the leader still held. `[modes."*"]` is bound
  into every non-passthrough mode before the mode's own table, so
  Esc/Enter leave any mode and specific keys win. Esc leaves even if
  unbound. A mode lost to a reload drops back to normal.
- **Physical keys.** The tap gives a hardware keycode, mapped through a
  US-ANSI table (`keymap/src/keycode.rs`), and device-independent
  modifier bits. Shifted glyphs fold to base key + shift: `"?"` is
  `shift+/`, `"H"` is `shift+h`, and `alt+shift+1` is its own chord.
- **Which-key.** `Level::Top` or `Level::Mode`. The leader pressed
  alone arms the clock (`which_key_delay_ms`, 300 ms; 0 opens at once);
  a key resolves it; releasing the leader closes it unless pinned
  (`help`, `alt+/`). In a mode each key re-arms a closed sheet, so a
  fluent run never flashes it.
- **Swallowing.** Bound keys are swallowed; unbound keys in a mode are
  swallowed unless the mode is passthrough; a key-up whose key-down was
  swallowed is swallowed once. Losing trust resets to normal mode.
- **Failure is visible.** A failed key action logs and shows its error
  in the status item for 2.5 s: `[1] · no window to the left`.

## Persistence

`~/.local/state/tatami/state.json` (`$TATAMI_STATE_DIR`,
`$XDG_STATE_HOME`), version 1: `version`, `saved_at`, `model`,
`focused_monitor` (uuid), and `monitors[]` each with `uuid`, `name`,
`frame`, `active` and `workspaces[]`; a workspace has `name`, `root`
(an untagged tree of `{layout, weight, children}` and `{weight,
window}`), `floating[] {window, frame}` and `focused` (DFS index). A
`SavedWindow` is `bundle_id`, `app`, `title`, `subrole`, `pid`,
`cg_id`, `size`. Writes are atomic (temp file + rename); an unreadable
file is moved to `state.corrupt.json`; a newer version is refused.

Save triggers: `Outcome::Dirty` saves 1 s after the last change (the
clock restarts on every change); every 5 s a periodic save runs but skips if the serialized
bytes are unchanged (compared before `saved_at` is stamped); `quit`,
SIGTERM and SIGINT force a save after un-parking; a hot swap saves
before it execs. Start-up with a saved layout runs the restore in place
of the first layout pass, so windows never see the interim tiling.

A restore (start-up, or `checkpoint restore`) is a session:

1. Map each saved monitor by uuid, else name, frame size, or main.
2. Detach every tiled window; rebuild each saved tree with `Pending`
   leaves, one spec per saved window, so the layout has its final shape
   before the windows exist.
3. Match live windows to specs globally best-first with
   `tatami_state::score`: bundle id (or app name) must match; +8 same
   pid and window id, +3 exact title or +2 normalised title, +1
   subrole, +1 size. Ties go to the earlier spec in tree order.
   Unmatched windows return to their monitor's visible workspace.
4. Launch each missing app once per bundle id, unless running,
   `restore.launch_apps = false` or listed in `restore.ignore`.
5. Each new window tries the remaining specs first. An app's specs
   expire `restore.timeout_s` (60 s) after its last sign of life and
   their placeholders are pruned. The session ends at zero pending.

A window that floats by rule or size stays floating even when saved in
a tiled slot. A snapshot taken mid-session keeps the pending specs.
Checkpoints are the same schema in `checkpoints/<name>.json`
(`auto-<secs>` when unnamed), handled by the daemon: the CLI path is
answered there directly, and a key or menu `checkpoint` reaches it as
`Outcome::Checkpoint`.

## IPC

Socket: `$TATAMI_SOCK`, else `$XDG_RUNTIME_DIR/tatami/sock`, else
`~/.tatami/run/sock`. Directory 0700, socket 0600, and the peer's uid
must equal the daemon's. A stale socket is detected by connecting.
Frames are `u32` little-endian length + JSON, capped at 16 MiB.

`Request { id, args, protocol }` gets `Response { id, ok, stdout,
error, data }`. `subscribe <topic>…` gets an ack, then `IpcEvent`
frames tagged `event`: `workspace`, `focus`, `mode`, `monitors`,
`restore`, `config`; a subscriber whose send fails is dropped.
`PROTOCOL_VERSION` (1) bumps on any shape change to these types. A
mismatch is an error naming both versions, never an auto-restart:
killing the WM is worse than a stale CLI. `protocol: 0` (a hand-made
client) is accepted.

## Platform layer notes

- **Coordinates.** Everything above the platform crate is AX space:
  global, top-left origin on the primary display. `CGDisplayBounds` is
  already in it; AppKit values (`NSScreen.visibleFrame`, the mouse, the
  overlay's frame) are flipped against the main display's height at the
  one place each crosses the boundary.
- **Identity.** `_AXUIElementGetWindow` gives the `CGWindowID`, which
  is the `WindowHandle` and matches the window server's list.
- **Observers.** One `AXObserver` per pid: `AXWindowCreated` (the
  only one whose failure fails registration), `AXUIElementDestroyed`,
  `AXFocusedWindowChanged`, `AXWindowMoved`, `AXWindowResized`,
  `AXWindowMiniaturized`, `AXWindowDeminiaturized`, `AXTitleChanged`,
  `AXApplicationActivated`. Plus NSWorkspace launch/terminate/activate,
  screen-parameter changes, and a 2 s trust poll while untrusted.
- **Key tap.** Session tap, head insert, `Default` (can swallow),
  key down/up and flags changed. `TapDisabledByTimeout` /
  `TapDisabledByUserInput` re-enable it; that key is lost.
- **Overlay.** One borderless `NonactivatingPanel` at
  `NSStatusWindowLevel + 1`, on all Spaces and over fullscreen, ignoring
  the mouse, not hidden on deactivate; one monospaced label,
  bottom-centre of the target monitor.
- **Status item.** An SF Symbol plus `[ws] MODE` text; the menu lists
  each monitor's workspaces and the service actions, and a pick emits
  `PlatformEvent::Command(argv)`.
- **Writes.** `AXEnhancedUserInterface` is switched off around a frame
  write (Chromium animates while it is on). The AX timeout is 1 s, not
  6, so a hung app cannot stall the loop long enough to lose the tap.

## Testing

`cargo test --workspace --features tatami-core/fake` runs about 200
tests with no permissions. The pure crates test logic directly (tree,
layout, workspaces, parking, keymap, which-key clock, config, matcher,
codec). `FakePlatform` misbehaves like real apps: `Behaviour {
ignore_frames, min_size, grid }`, `set_resizable`, `user_set_frame` (a
hand drag), `set_native_fullscreen`, `set_screens`, `set_mouse`. It
logs every call (`FakeCall`) and queues the events macOS would send
back. In `reactor/tests.rs`, `settle()` pumps events and runs every
timer due within a second until quiet; `calls()` takes the log, so a
test can assert one write per window or the `SetOrder` used.

The platform crate is checked by hand with `cargo run -p
tatami-platform --example probe -- <monitors|windows|move|focus|events|
keys|overlay|status|menu|front|apps|…>`. `scripts/smoke.sh [secs]` runs
a debug daemon on its own socket and state dir, drives it through the
CLI, SIGTERMs it and greps the log. A test for a fix must fail without
the fix: sabotage once and watch it go red.

## Lessons

- **Chrome refuses a grow from the old position.** Writes go position,
  size, position; a later retry showed as a visible two-step.
- **AX reports success for ignored writes**, so every write is read back.
- **Minimums belong to the layout.** Bumping a stubborn window's weight
  piled up across flips (each makes or dissolves a container) until the
  row drifted off the monitor.
- **Retries are visible.** 60/150/400 ms made a window at its minimum
  twitch for half a second; now 40/90/250 ms with an early stop.
- **Fixed-size windows cannot tile.** System Settings even claims a
  settable size but pins its width at 736, hence the built-in rules.
- **Restore must not overrule a float rule**: a saved tiled slot put
  System Settings back beside the terminal and squeezed it.
- **AX sends `WindowFocused` before `WindowCreated`**: `pending_focus`.
- **plyterm's hot swap leaves a pid -1 NSWorkspace record** and the live
  pid nowhere, hence the window-server scan and `AXApplicationActivated`.
- **The first key did nothing** with nothing focused at start, hence
  `default_focus()` and the status flash.
- **A hand drag snapped back on its first move event**, so no window
  could cross monitors; hence the 200 ms settle.
- **The installer hung under `install.sh | tee`** because the daemon
  inherited its stdio; the subshell now closes it before `exec`.
- **The Accessibility grant follows the code signature**, so the
  installer signs every build with one `tatami-codesign` identity.
