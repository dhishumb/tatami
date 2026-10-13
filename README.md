# tatami

A tiling window manager for macOS in the spirit of AeroSpace, written in
Rust, small and fast. It uses only the public Accessibility API: no SIP
changes, no injected code. One daemon (`tatamid`) owns the windows; one
thin CLI (`tatami`) talks to it over a Unix socket.

What it does differently from AeroSpace:

- **Workspaces belong to monitors, and windows get their own.** Each
  monitor has its own `1`…`9`; `alt+3` switches *this* monitor's 3, and
  two monitors can both show a `2`. A new window takes the lowest empty
  workspace on its monitor and fills it; tiling is a deliberate move of
  one window into another's workspace (`alt+shift+8`). AeroSpace's shared
  pool and i3's tile-on-open are each one config line away.
- **plyterm-style keys.** The chord you press is the prefix. One-press
  Alt chords for the every-minute actions, four group chords
  (`alt+w` workspaces, `alt+m` move, `alt+r` resize, `alt+s` service) that
  open a mode, and a which-key sheet that lists the keys when you pause.
  Nothing is three keys.
- **Layouts survive a reboot.** The tree is saved as readable JSON a
  second after every change. On start, missing apps are relaunched and
  their windows slotted back as they appear. Named checkpoints too.

## Install

```sh
git clone https://github.com/dhishumb/tatami.git && cd tatami
scripts/install.sh            # build, install to ~/.local/bin, sign, start at login, run
```

The first run asks for Accessibility; tick `tatamid` in System Settings →
Privacy & Security → Accessibility and run `tatami service restart`. The
binaries are signed with a self-signed identity created once in your login
keychain, so that grant survives upgrades. `scripts/install.sh --upgrade`
reinstalls the binaries and hot-swaps the running daemon onto the new
build: same pid, same socket, no window moves (`tatami hot-swap` or
`alt+s u` does it by hand). See the script header for `PREFIX`,
`SKIP_BUILD`, `TATAMI_NO_SERVICE` and the other knobs.

Quit Rectangle, yabai or AeroSpace first; two managers fight.
`tatami doctor` checks the socket, the process and known conflicts.

## Keys (default preset)

| chord | action |
|---|---|
| `alt+h/j/k/l` | focus left / down / up / right (crosses monitors at the edge) |
| `alt+n` / `alt+p` | next / previous tile on the workspace, wrapping around |
| `alt+shift+h/j/k/l` | move the window (crosses monitors at the root edge) |
| `alt+←/→/↑/↓` | resize the focused window by 50 px (width, height) |
| `alt+shift+←/→/↑/↓` | move the window, same as `alt+shift+h/j/k/l` |
| `alt+v` / `alt+b` | split: the next window opens below / beside the focused one |
| `alt+1`…`alt+9` | workspace N on the focused monitor |
| `alt+shift+1`…`9` | move the window to workspace N and follow |
| `alt+tab` | workspace back and forth |
| `alt+f` | fullscreen toggle |
| `alt+t` | floating / tiling toggle |
| `alt+e` | flip: this window and its neighbour, side by side ↔ stacked (press again to undo) |
| `alt+shift+e` | the whole container: horizontal ↔ vertical |
| `alt+a` | accordion: horizontal ↔ vertical |
| `alt+/` | pin the which-key sheet |
| `alt+o` | overview on the main monitor, an outline of monitors: `↑↓` move, `→` expands a monitor, `←` collapses; `Enter` or a letter focuses a window, `/` searches windows and keys (`Enter` on a key runs it) |
| `alt+w` | **workspaces**: `h`/`l` prev/next, `n`/`p` next/prev monitor, `m` move workspace to next monitor, `1-9` |
| `alt+m` | **move** (sticky): `hjkl` move, `HJKL` move to monitor, `n` swap next, `1-9` move to workspace |
| `alt+r` | **resize** (sticky): `h`/`l` width, `j`/`k` height, `-`/`+` smart, `=` balance |
| `alt+s` | **service**: `r` reload config, `f` flatten, `c` checkpoint, `e` enable toggle, `b` balance, `u` hot-swap to the installed build, `q` quit |

Hold Alt alone for 300 ms and the sheet lists the direct chords; pause
inside a group and it lists that group. `Esc`/`Enter` leave any mode.
Unbound Alt chords pass through to the app; inside a mode every key is
captured until you leave.

**Control is where the pointer is.** A chord or command acts on the monitor
under the mouse, and the key sheet opens there. If the focused window is on
another monitor, the pointer's monitor's last-focused window takes over.
When a key moves focus to another monitor (`alt+w n`, `alt+l` past the
edge), the pointer comes along, so the keys and the rule agree.

## Config

`~/.config/tatami/config.toml` (hot-reloaded). Everything is optional:

```toml
[general]
which_key_delay_ms = 300
leader = "alt"

[gaps]
inner = 8
outer = 8                      # or { top = 8, bottom = 8, left = 8, right = 8 }

[layout]
default = "h_tiles"            # h_tiles | v_tiles | h_accordion | v_accordion
accordion_padding = 30

[workspaces]
model = "per-monitor"          # or "shared" (AeroSpace semantics)
names = ["1","2","3","4","5","6","7","8","9"]
persistent = []                # never garbage-collected when empty

[restore]
enabled = true
launch_apps = true
timeout_s = 60
ignore = []                    # bundle ids never relaunched

[[rules]]                      # yours are checked first; built-in rules float System Settings,
app_id = "com.apple.calculator" # Calculator, Archive Utility, Installer, Digital Color Meter
float = true                   # or workspace = "3", or ignore = true

[keys]
preset = "tatami"              # built-in, or ~/.config/tatami/keymaps/<name>.toml
"alt+enter" = "exec-and-forget open -a Ghostty"
[keys.modes.resize]
"x" = ["balance-sizes", "normal"]
```

Keymap files have plyterm's shape: `"chord" = "command"` or a list,
`[modes."*"]` for every mode, `[modes.<name>]` for one, `extends =
"tatami"` to start from a preset. A list ending in `"normal"` is one-shot;
otherwise the mode is sticky. `tatami keymap tatami` prints the preset.

## CLI

Any command you can bind, you can run: `tatami focus left`,
`tatami workspace 3`, `tatami layout tiles horizontal vertical`,
`tatami list-windows --json`, `tatami reload-config`, `tatami quit`.
Names follow AeroSpace. Plus:

- `tatami checkpoint save [name]` / `restore <name>` / `list` / `delete <name>`
- `tatami subscribe workspace focus mode` streams JSON lines (for
  sketchybar and friends; `--all` for every topic)
- `tatami ping` reports trust and counts

## How it works

Hidden workspaces park their windows with one point still on their own
monitor (macOS refuses fully off-screen moves); quitting un-parks
everything, and a restart after a crash un-parks strays. Frame changes are
transactions: write position-size-position, read back, retry at 40, 90 and
250 ms, and a window that keeps refusing a size keeps it (apps with minimum
widths); the layout then treats that size as a minimum.
All state lives on the main thread's run loop; the socket, signals and
config watcher are std threads that poke it.

State: `~/.local/state/tatami/state.json`, checkpoints in
`checkpoints/`. Socket: `$XDG_RUNTIME_DIR/tatami/sock` or
`~/.tatami/run/sock`. Logs with `RUST_LOG=debug tatamid`.

## Development

```sh
cargo clippy --workspace --all-targets --features tatami-core/fake   # silent
cargo test --workspace --features tatami-core/fake                   # ~200 tests, no permissions needed
scripts/install.sh --upgrade                                         # install + restart the daemon on this build
scripts/smoke.sh 30                                                  # isolated live run for 30 s
cargo run -p tatami-platform --example probe -- windows              # raw platform probe
```

## Documentation

- [docs/USER_GUIDE.md](docs/USER_GUIDE.md): day-to-day recipes, every key, configuration, scripting, diagnostics.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): how the code is organised and why.
- [docs/CODE_SIGNING.md](docs/CODE_SIGNING.md): why the installer signs the binaries and how to opt out.
- [docs/RELEASING.md](docs/RELEASING.md): cutting a release.
- [CHANGELOG.md](CHANGELOG.md): what changed, for the user.
- `CLAUDE.md`: the rules that are easy to get wrong, for anyone (or any agent) changing the code.
