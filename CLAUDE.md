# tatami — working notes for coding agents

tatami is a tiling window manager for macOS: an AeroSpace-like daemon
(`tatamid`) and a thin CLI (`tatami`), in Rust, using only the public
Accessibility API (no SIP changes). The user's standing requirement is
**speed and lightness**: std over frameworks, parse once at load, no
dependency for a small need. This file is the rules that are easy to get
wrong; the design is in `docs/` and the plan that started it.

## The loop

```sh
cargo fmt --all --check && cargo clippy --workspace --all-targets --features tatami-core/fake   # silent
cargo test --workspace --features tatami-core/fake                                            # all green
scripts/install.sh --upgrade                                                                   # install, sign, restart the daemon
```

- Never fix clippy with `#[allow]`; restructure.
- Quit Rectangle (and any other WM) before manual tests; two managers fight
  over every frame.
- `tatamid` must be signed with the same identity across builds
  (`scripts/install.sh` does it with the `tatami-codesign` keychain
  identity) or macOS forgets the Accessibility grant.
- Install with `scripts/install.sh`, never by copying binaries: it keeps
  the hash sidecars that decide whether the daemon needs a restart, and
  it restarts it cleanly (`tatami quit` saves and un-parks first).

## Crates and the dependency direction

`keymap` depends on nothing; `protocol ← config` (and `keymap ← config`);
`protocol ← state`; `core` depends on protocol, keymap, config and state;
`platform ← core`; everything `← tatamid`; `tatami` (CLI) depends on
`protocol` and `config`.

- **protocol**: `Action` is the single vocabulary for keybindings and the
  CLI. Add commands there first; the parser is the contract. Names follow
  AeroSpace so configs port.
- **core** is pure: no macOS types, tested against `fake::FakePlatform`
  (`--features fake`). The `Platform` trait in `core/src/platform.rs` is
  the only door to the OS. Coordinates are AX coordinates (top-left
  origin); the platform crate flips AppKit frames once at its boundary.
- **platform** is where `unsafe` lives, as thin wrappers with a one-line
  `// SAFETY:` each; the only exceptions are three libc calls for uids in
  `tatamid/src/ipc.rs` and `tatami/src/service.rs`. Everything in platform
  runs on the main thread's run loop.
- **keymap** is generic over the action type and has no OS deps.

## Rules learned from real behaviour

- AX frame writes return success even when the app ignores them. Always
  read back. Every write is position→size→position (`PosSizePos`): Chrome
  clamps a grow written size-first from the old position, and the three
  writes cost less than one visible retry. Verify at 40/90/250 ms; a
  window that answers two writes with the same size at the right position
  is "stubborn" after the second, and only the refused dimension becomes
  a minimum. Don't add special cases per app.
- `AXWindows` only lists windows on the current Space. We assume one
  Space per monitor.
- Keys are **physical**: the tap gives a keycode and a shift flag, so
  `alt+shift+1` is its own chord and `"+"` means shift+`=`. plyterm's
  "punctuation ignores shift" rule does not apply here.
- The event-tap callback must stay fast and must not call AX: queue and
  return. A slow callback gets the tap disabled by macOS (we re-enable,
  but the key is lost).
- A consuming tap needs Accessibility trust; install it only after
  `AXIsProcessTrusted()` and reinstall on `TrustChanged(true)`.
- macOS cannot be trusted about focus: `NSWorkspace` keeps a stale
  pid -1 record for an app that re-exec'd itself (plyterm) and never
  lists the live pid; the system-wide `AXFocusedApplication` fails; the
  window server's front window can be a floating utility window (Google
  Drive). Apps are therefore enumerated from `CGWindowListCopyWindowInfo`
  owners too, per-app `AXApplicationActivated` is observed, and before
  any chord or command runs `follow_mouse()` makes the pointer's monitor
  the focused one. Don't "fix" focus by trusting one source again.
- A window whose app returns an AX *error* on a frame write (not merely
  ignores it) is floated automatically; it cannot be tiled.
- Hidden workspaces park windows with exactly 1pt still on their own
  monitor (`parking.rs`); the corner is chosen so nothing peeks onto a
  neighbour. On quit/SIGTERM everything is un-parked. After a `kill -9`
  the next start un-parks strays.

## Daemon shape (`crates/tatamid`)

- One `RefCell<Daemon>`; every reactor call goes through `step()`, which
  borrows once, handles `Outcome`s, re-arms the single timer, and only
  then does the things that need the `Rc` (reload, exit, tap install).
  The one exception is the key tap: it borrows directly for `on_key`
  (synchronous swallow decision) and passes the key through if the
  daemon is busy, then queues a `drain` through the run loop.
- Platform callbacks only queue and poke; the socket, signal and watcher
  threads only send on the channel and call the waker.
- Persistence: `Outcome::Dirty` → save 1 s after the last change (the
  clock restarts on every change), plus a 5 s periodic save skipped when
  the bytes are unchanged, plus on exit.
  Nothing expensive on this path.
- **Hot swap.** Never `exec` without the `--version` preflight (`tatamid …`
  plus a `handoff N` line: a build from before hot swap ignores `--adopt`,
  finds the socket taken and exits, taking the WM with it) and a written
  manifest. Only the listening socket crosses the exec, as standard
  input; the manifest is JSON, fields added with `#[serde(default)]`,
  `HANDOFF_FORMAT` bumped only for a shape an old reader must refuse.
  The installer asks the daemon (`tatami hot-swap`) and falls back to the
  cold restart on `unknown command` or `failed`. By hand: `kill -USR1
  <exact pid>`, never `pkill -USR1 -x tatamid` (SIGUSR1 kills a
  pre-hot-swap tatamid).

## Protocol

- Frames are `u32 LE length + JSON`. `PROTOCOL_VERSION` bumps on any
  shape change to `Request`/`Response`/`IpcEvent`; a mismatch is an error
  the CLI prints, never an auto-restart (killing the WM is worse than a
  stale CLI).
- State file `state.json` is version 1; add fields with
  `#[serde(default)]`, never repurpose one.

## Tests

- Logic is tested as pure functions (tree, layout, workspace model,
  keymap dispatch, which-key clock, matcher, codec). A test for a fix must
  fail without the fix: sabotage once and watch it go red.
- The platform crate is verified with `cargo run -p tatami-platform
  --example probe -- <monitors|windows|move|events|keys|overlay>`.

## Commits and docs

- Every user-visible change gets a `CHANGELOG.md` `[Unreleased]` entry in
  the same commit, in the existing voice: what was wrong, what changed,
  why.
- After any binding change: `README.md` (keys table), `docs/USER_GUIDE.md`
  (Keys table and the recipes), the preset's header comments and
  `scripts/install.sh`'s next-steps text are hand-written and go stale.
  Grep for the chord and check every claim against `tatami keymap`.
  The which-key sheet and `tatami keymap` are generated and need nothing.
- `docs/ARCHITECTURE.md` describes mechanisms, not history; update it
  when a mechanism changes (focus rules, frame transactions, parking,
  restore), not for every fix.

- One focused commit per change after the loop passes. Subject:
  `scope: lowercase summary` (`core:`, `platform:`, `keys:`, `daemon:`,
  `cli:`, `docs:`). Body says why.
- Comments explain why, at the density of the surrounding code.
