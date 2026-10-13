#!/usr/bin/env bash
# tatami install script.
#
# Installs two binaries: tatamid (the window manager daemon) and tatami
# (the CLI). Same shape as plyterm's and kludge's installers.
#
# Two install modes:
#
#   default   Full install — build (or reuse) release binaries, install
#             and sign them, seed ~/.config/tatami/config.toml if missing,
#             install the launchd agent so tatamid starts at login, start
#             (or restart) the daemon, print the next steps. Safe to re-run.
#
#   --upgrade Binary-only — install and sign the binaries, move a running
#             daemon onto the new build, and stop. No config or launchd
#             changes.
#
# A running tatamid is touched only when its binary changed, and then it
# is hot-swapped: `tatami hot-swap` makes the daemon exec the new binary
# over itself (same pid, same socket, no window moves; the installer waits
# for the new image's `ok` in swap-status next to the socket). A daemon
# from before hot swap, or a swap that fails, gets the cold restart:
# `tatami quit` saves the layout and un-parks every window, then launchd
# (or this script) starts the new build, which puts every window back
# from state.json. TATAMI_NO_RELAUNCH=1 leaves a running daemon alone.
#
# Sources for the binaries (checked in order):
#
#   1. $TATAMI_BIN            # directory containing tatamid and tatami
#   2. target/release/*       # output of `cargo build --release`
#   3. cargo build --release  # fresh build (skipped if SKIP_BUILD=1)
#
# Override targets:
#   PREFIX=/usr/local          ./scripts/install.sh   # install to /usr/local/bin
#   SKIP_BUILD=1               ./scripts/install.sh   # reuse target/release/*
#   TATAMI_BIN=./bundle        ./scripts/install.sh --upgrade
#   TATAMI_SKIP_CODESIGN=1     ./scripts/install.sh   # don't sign (Accessibility re-prompts per build)
#   TATAMI_CODESIGN_CERT=name  ./scripts/install.sh   # sign with your own identity
#   TATAMI_NO_SERVICE=1        ./scripts/install.sh   # don't install the launchd agent
#   TATAMI_NO_RELAUNCH=1       ./scripts/install.sh   # don't touch a running daemon
#
# Why sign: macOS ties the Accessibility grant to the code signature. An
# unsigned (ad-hoc) build has a new identity every time, so the checkbox
# would need re-ticking after every install. A self-signed identity in
# your login keychain, created once here, keeps the grant.

set -euo pipefail

MODE="full"
for arg in "$@"; do
    case "$arg" in
        --upgrade) MODE="upgrade" ;;
        --full)    MODE="full" ;;
        -h|--help)
            sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            printf '\033[33m⚠\033[0m unknown arg: %s (see --help)\n' "$arg" >&2
            exit 1
            ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/tatami"
STATE_DIR="${TATAMI_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/tatami}"
LABEL="com.vgupta.tatami"

BINS=(tatamid tatami)

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
info() { printf '\033[36m▸\033[0m %s\n' "$*"; }
ok()   { printf '\033[32m✓\033[0m %s\n' "$*"; }
warn() { printf '\033[33m⚠\033[0m %s\n' "$*" >&2; }

if [[ "$(uname -s)" != "Darwin" ]]; then
    warn "tatami is a macOS window manager; nothing to install here"
    exit 1
fi

# 1. Locate or build the binaries.
SOURCE_DIR=""
if [[ -n "${TATAMI_BIN:-}" ]]; then
    if [[ -d "$TATAMI_BIN" ]]; then
        SOURCE_DIR="$TATAMI_BIN"
    elif [[ -x "$TATAMI_BIN" ]]; then
        SOURCE_DIR="$(dirname "$TATAMI_BIN")"
    else
        warn "TATAMI_BIN=$TATAMI_BIN — not a dir or executable file"
        exit 1
    fi
    info "using prebuilt binaries from $SOURCE_DIR"
elif [[ "${SKIP_BUILD:-0}" == "1" ]]; then
    SOURCE_DIR="target/release"
    info "SKIP_BUILD=1 — reusing target/release/{tatamid,tatami}"
else
    if ! command -v cargo >/dev/null 2>&1; then
        warn "cargo not found on PATH — install Rust via https://rustup.rs"
        warn "or pass TATAMI_BIN=<dir-with-binaries> to skip the build step"
        exit 1
    fi
    info "building release binaries…"
    cargo build --release -p tatamid -p tatami
    SOURCE_DIR="target/release"
    ok "built target/release/{tatamid,tatami}"
fi
for b in "${BINS[@]}"; do
    if [[ ! -x "$SOURCE_DIR/$b" ]]; then
        warn "binary not found at $SOURCE_DIR/$b"
        exit 1
    fi
done

# 2. Install, remembering each unsigned build's hash: the installed copy
# is re-signed below, so its bytes never match a fresh build, and the
# hash is what tells the relaunch step whether the daemon changed.
binary_changed() {
    local src="$1" sidecar="$2" new old
    new="$(shasum -a 256 "$src" 2>/dev/null | awk '{print $1}')" || return 0
    [[ -n "$new" && -r "$sidecar" ]] || return 0
    old="$(cat "$sidecar" 2>/dev/null)" || return 0
    [[ "$new" != "$old" ]]
}

mkdir -p "$BIN_DIR"
DAEMON_CHANGED=0
for b in "${BINS[@]}"; do
    sidecar="$BIN_DIR/.$b.sha256"
    changed=0
    if binary_changed "$SOURCE_DIR/$b" "$sidecar"; then changed=1; fi
    [[ "$b" == "tatamid" ]] && DAEMON_CHANGED=$changed
    install -m 755 "$SOURCE_DIR/$b" "$BIN_DIR/$b"
    if hash="$(shasum -a 256 "$SOURCE_DIR/$b" 2>/dev/null | awk '{print $1}')" && [[ -n "$hash" ]]; then
        printf '%s\n' "$hash" > "$sidecar" || rm -f "$sidecar"
    else
        rm -f "$sidecar"
    fi
    if [[ "$changed" == "1" ]]; then
        ok "installed $BIN_DIR/$b"
    else
        ok "installed $BIN_DIR/$b (unchanged)"
    fi
done

# 3. Sign with a stable self-signed identity (see the header). The cert
# shape and keychain steps are the ones plyterm's installer settled on:
# non-critical keyUsage/extendedKeyUsage, PKCS#12 with a fixed local
# password, user-domain trust (one password/Touch ID prompt, once).
create_codesign_cert() {
    local cert="$1"
    info "creating '$cert' self-signed code-signing cert (one-time)…"
    info "macOS may ask for your password to mark it as trusted."
    local ossl="openssl"
    if [[ -x /usr/bin/openssl ]]; then ossl="/usr/bin/openssl"; fi
    local tmpdir conf
    tmpdir="$(mktemp -d)"
    conf="$tmpdir/openssl.cnf"
    cat > "$conf" <<EOF
[req]
distinguished_name = req_dn
prompt = no
x509_extensions = v3_codesign

[req_dn]
CN = $cert

[v3_codesign]
basicConstraints = CA:FALSE
keyUsage = digitalSignature
extendedKeyUsage = codeSigning
subjectKeyIdentifier = hash
EOF
    if ! "$ossl" req -x509 -newkey rsa:2048 -nodes -days 3650 \
            -keyout "$tmpdir/key.pem" -out "$tmpdir/cert.pem" \
            -config "$conf" >/dev/null 2>&1; then
        warn "openssl req failed — cert NOT created"; rm -rf "$tmpdir"; return 1
    fi
    local p12_pass="tatami-codesign-installer"
    if ! "$ossl" pkcs12 -export -inkey "$tmpdir/key.pem" -in "$tmpdir/cert.pem" \
            -out "$tmpdir/bundle.p12" -password "pass:$p12_pass" -name "$cert" >/dev/null 2>&1; then
        warn "openssl pkcs12 failed — cert NOT created"; rm -rf "$tmpdir"; return 1
    fi
    local login_kc="$HOME/Library/Keychains/login.keychain-db"
    if ! security import "$tmpdir/bundle.p12" -k "$login_kc" -P "$p12_pass" -A \
            -T /usr/bin/codesign -T /usr/bin/security >/dev/null 2>&1; then
        warn "security import failed — cert NOT created"; rm -rf "$tmpdir"; return 1
    fi
    security add-trusted-cert -r trustRoot -p codeSign -k "$login_kc" "$tmpdir/cert.pem" >/dev/null 2>&1 \
        || warn "trust setup didn't complete (re-run install.sh to retry)"
    rm -rf "$tmpdir"
    if security find-identity -p codesigning -v 2>/dev/null | grep -q "\"$cert\""; then
        ok "created '$cert' in your login keychain"
        return 0
    fi
    warn "cert imported but not visible as a code-signing identity"
    return 1
}

sign_for_tcc() {
    if [[ "${TATAMI_SKIP_CODESIGN:-0}" == "1" ]]; then
        info "TATAMI_SKIP_CODESIGN=1 — skipping signing (Accessibility will re-prompt after each install)"
        return
    fi
    local cert="${TATAMI_CODESIGN_CERT:-tatami-codesign}"
    if ! security find-identity -p codesigning -v 2>/dev/null | grep -q "\"$cert\""; then
        if [[ "$cert" == "tatami-codesign" ]]; then
            create_codesign_cert "$cert" || return
        else
            info "no '$cert' code-signing identity in your login keychain — skipping signing"
            return
        fi
    fi
    info "signing binaries with '$cert' (the Accessibility grant will persist)…"
    local failed=0
    for b in "${BINS[@]}"; do
        if ! codesign --force --sign "$cert" --identifier "$LABEL" "$BIN_DIR/$b" 2>/dev/null; then
            warn "codesign failed for $BIN_DIR/$b"; failed=1
        fi
    done
    [[ "$failed" == "0" ]] && ok "signed binaries with '$cert'"
}
sign_for_tcc

# 4. Smoke test and PATH shadow check.
if version_out="$("$BIN_DIR/tatami" --version 2>&1)"; then
    ok "smoke test: $version_out"
else
    warn "smoke test failed — \`$BIN_DIR/tatami --version\` did not run cleanly"
fi
if resolved="$(command -v tatami 2>/dev/null)" && [[ -n "$resolved" ]] && ! [[ "$resolved" -ef "$BIN_DIR/tatami" ]]; then
    echo
    warn "\`tatami\` on your PATH is NOT the one just installed:"
    warn "    runs:      $resolved"
    warn "    installed: $BIN_DIR/tatami"
    warn "Put $BIN_DIR earlier in PATH, or remove the other copy."
fi

# 5. The running daemon. Hot swap first: the daemon execs the new binary
# in place and the new image writes `ok <version>` to swap-status next to
# the socket (the old one writes `failed <why>` and keeps serving). A
# daemon that does not know the command, or a failed swap, falls back to
# the cold restart: via launchd when the agent is installed (it respawns
# the new build), else by hand; `quit` saves state and un-parks every
# window first, and the new build puts them back from state.json.
service_loaded() {
    launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1
}
daemon_running() {
    pgrep -a -x tatamid >/dev/null 2>&1
}
hot_swap_daemon() {
    local sock status out line pid
    sock="$("$BIN_DIR/tatami" socket-path 2>/dev/null)" || return 1
    status="${sock%/*}/swap-status"
    pid="$(pgrep -x tatamid | head -1)"
    rm -f "$status"
    if ! out="$("$BIN_DIR/tatami" hot-swap 2>&1)"; then
        case "$out" in
            *"unknown command"*)
                info "the running daemon predates hot swap — restarting it instead"
                return 1 ;;
            *) ;;   # the reply can be cut by the exec itself; the status file decides
        esac
    fi
    for _ in $(seq 1 40); do
        if [[ -s "$status" ]]; then
            read -r line < "$status"
            case "$line" in
                ok*) ok "hot-swapped the running daemon to tatamid ${line#ok } (same pid $pid, no window moved)"; return 0 ;;
                *)   warn "hot swap failed: ${line#failed }"; return 1 ;;
            esac
        fi
        sleep 0.25
    done
    warn "no word from the daemon after 10 s — see $STATE_DIR/log/tatamid.log"
    return 0
}
restart_daemon() {
    if [[ -n "${TATAMI_NO_RELAUNCH:-}" ]]; then return; fi
    if ! daemon_running; then return; fi
    if [[ "$DAEMON_CHANGED" != "1" ]]; then
        info "the running daemon is unchanged — nothing to restart"
        return
    fi
    if hot_swap_daemon; then return; fi
    info "restarting the running daemon on the new build (layout is saved and restored)…"
    "$BIN_DIR/tatami" quit >/dev/null 2>&1 || pkill -TERM -x tatamid 2>/dev/null || true
    for _ in $(seq 1 40); do
        daemon_running || break
        sleep 0.25
    done
    if service_loaded; then
        launchctl kickstart "gui/$(id -u)/$LABEL" >/dev/null 2>&1 || true
        ok "launchd restarted tatamid"
    else
        start_daemon_by_hand
    fi
}
# The daemon must answer before this script calls itself done: a launchd
# job that never came up, or a build without Accessibility, is the first
# thing a new install runs into, and silence here is what makes it look
# like "the install did not start the server".
verify_daemon() {
    local reply=""
    for _ in $(seq 1 40); do
        if reply="$("$BIN_DIR/tatami" ping 2>/dev/null)"; then break; fi
        reply=""
        sleep 0.25
    done
    if [[ -z "$reply" ]]; then
        warn "tatamid is not answering on its socket after 10 s — see $STATE_DIR/log/tatamid.log"
        warn "and \`tatami service status\`; \`tatami service start\` starts it"
        return
    fi
    ok "tatamid is running: ${reply#pong: }"
    case "$reply" in
        *"NOT granted"*)
            warn "tatamid cannot manage windows until it is ticked under System Settings →"
            warn "Privacy & Security → Accessibility (opening it now); the daemon notices by itself"
            open "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility" 2>/dev/null || true
            ;;
    esac
}
start_daemon_by_hand() {
    mkdir -p "$STATE_DIR/log"
    # Fully detached: no inherited stdio, or a pipeline running this script
    # (`install.sh | tee`) would wait on the daemon forever.
    # The subshell closes its own stdio before exec'ing the daemon, so a
    # pipeline running this script never waits on tatamid.
    (
        exec </dev/null >>"$STATE_DIR/log/tatamid.log" 2>&1
        cd "$HOME" && RUST_LOG="${RUST_LOG:-info}" exec nohup "$BIN_DIR/tatamid"
    ) &
    disown 2>/dev/null || true
    ok "started tatamid (log: $STATE_DIR/log/tatamid.log)"
}

if [[ "$MODE" == "upgrade" ]]; then
    restart_daemon
    case ":${PATH}:" in
        *":${BIN_DIR}:"*) ;;
        *) echo; warn "$BIN_DIR is not on your PATH. Add it:"; echo "    export PATH=\"$BIN_DIR:\$PATH\"" ;;
    esac
    echo
    bold "tatami upgraded."
    exit 0
fi

# 6. Seed the config dir. tatami runs on built-in defaults without a file;
# the template (every option at its default, commented) is there to be
# edited — the menu's "Edit config…" opens it.
mkdir -p "$CONFIG_DIR/keymaps" "$STATE_DIR/log"
CONFIG_FILE="$CONFIG_DIR/config.toml"
if [[ -f "$CONFIG_FILE" ]]; then
    info "$CONFIG_FILE already exists — leaving it alone"
else
    "$BIN_DIR/tatami" config-template > "$CONFIG_FILE"
    ok "wrote default config to $CONFIG_FILE"
fi

# 7. Start at login, and run now.
if [[ "${TATAMI_NO_SERVICE:-0}" == "1" ]]; then
    info "TATAMI_NO_SERVICE=1 — not installing the launchd agent"
    if daemon_running; then restart_daemon; else start_daemon_by_hand; fi
elif service_loaded; then
    restart_daemon
    ok "launchd agent already installed ($LABEL)"
else
    if daemon_running; then
        # Hand it over to launchd: stop the manual one first.
        "$BIN_DIR/tatami" quit >/dev/null 2>&1 || true
        for _ in $(seq 1 40); do daemon_running || break; sleep 0.25; done
    fi
    if "$BIN_DIR/tatami" service install >/dev/null 2>&1; then
        ok "installed launchd agent $LABEL (starts at login, restarts after a crash)"
    else
        warn "launchd agent install failed — \`tatami service install\` for the error"
        start_daemon_by_hand
    fi
fi
verify_daemon

# 8. Next steps.
echo
bold "tatami installed."
echo
case ":${PATH}:" in
    *":${BIN_DIR}:"*) ;;
    *) warn "$BIN_DIR is not on your PATH. Add it:"; echo "    export PATH=\"$BIN_DIR:\$PATH\""; echo ;;
esac
cat <<EOF
Next steps:

  - Accessibility:   if macOS asked, tick tatamid in System Settings →
                     Privacy & Security → Accessibility; the daemon notices
                     by itself. \`tatami ping\` reports it.
  - Try the keys:    alt+h/j/k/l focus, alt+shift+h/j/k/l move, alt+1..9
                     workspaces, alt+f fullscreen, alt+t float. Hold Alt
                     for the key sheet. Everything acts on the monitor
                     under the mouse.
  - All keys:        tatami keymap
  - Config:          $CONFIG_FILE    (hot-reloaded; every option is in
                     it, commented; \`tatami edit-config\` or the menu opens it)
  - Menu bar:        the grid icon lists workspaces per monitor and the
                     service actions (reload, edit config, checkpoint,
                     enable, start at login, hot-swap, quit)
  - Checkpoints:     tatami checkpoint save <name> | restore <name> | list
  - Desks:           one layout per set of monitors, switched when you dock
                     or undock; tatami desk list, or the menu

Layout:
  $CONFIG_FILE      config (+ keymaps/ for your own presets)
  $STATE_DIR/state.json   live layout, saved a second after every change
  $STATE_DIR/checkpoints/ named layouts
  $STATE_DIR/log/tatamid.log

Daemon: tatami service status | start | stop | restart     tatami quit (stays quit)
Start at login: tatami service enable | disable, or the menu bar item
Upgrade: git pull && ./scripts/install.sh --upgrade  (hot-swaps the daemon in place)
Uninstall: tatami service uninstall && rm $BIN_DIR/{tatamid,tatami}
           (and $CONFIG_DIR, $STATE_DIR for a clean slate)
EOF
