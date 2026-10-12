# Releasing tatami

Local-first, like plyterm and kludge: the maintainer builds on the Mac
they own, uploads with `gh`, and anyone else builds from source. There is
no release yet; this is the procedure for the first one and the ones
after it.

## When to bump

- **Patch** (`0.1.x → 0.1.y`): bug fixes, docs, refactors with no
  user-facing change.
- **Minor** (`0.x.0 → 0.y.0`): new chords, commands, config keys (with
  defaults), new menu items. Backward compatible; the state file still
  loads.
- **Any change to `Request`, `Response` or `IpcEvent`** in
  `crates/tatami-protocol/src/ipc.rs`: bump `PROTOCOL_VERSION` in
  `crates/tatami-protocol/src/lib.rs` in the same commit. A CLI and a
  daemon on different versions refuse each other with a message; the
  daemon never restarts itself over it.
- **Any change to `state.json`'s shape** (`crates/tatami-state/src/schema.rs`):
  add fields with `#[serde(default)]`; never repurpose one. A newer
  `version` than the daemon knows is refused and the file set aside as
  `state.corrupt.json`, so bump `STATE_VERSION` only for a shape an old
  daemon must not read.
- **Major** (`x.0.0 → y.0.0`): protocol or state breakage, renamed
  chords or commands.

Pre-1.0, breaking changes may land in minors; keep the CHANGELOG honest.

## Steps

### 1. Bump the version

One source of truth: `[workspace.package].version` in `Cargo.toml`; every
crate inherits it, and `Info.plist` carries `CFBundleShortVersionString`
by hand.

```sh
$EDITOR Cargo.toml crates/tatamid/Info.plist
cargo check --workspace        # refreshes Cargo.lock
```

### 2. CHANGELOG.md

Move the `[Unreleased]` entries under a new `## [x.y.z] — YYYY-MM-DD`
heading. Keep `### Added` / `### Changed` / `### Fixed` / `### Known
limitations`. Entries are written for the user: what was wrong, what
changed, why. Add an `### Upgrading` note when a release needs more than
`./scripts/install.sh --upgrade` (a config key renamed, a state file
reset).

### 3. Docs

`README.md` and `docs/USER_GUIDE.md` are hand-written and go stale:
grep both for every chord the release touched and check each claim
against `tatami keymap`. `CLAUDE.md` lists the other places that do.

### 4. The loop, then a live run

```sh
cargo fmt --all --check && cargo clippy --workspace --all-targets --features tatami-core/fake
cargo test --workspace --features tatami-core/fake
./scripts/install.sh --upgrade        # restarts your own daemon on the build
```

Use it for a while: switch workspaces on every monitor, flip, drag a
tile to another monitor, quit and restart (`tatami quit`, `tatami service
restart`) and watch the layout come back. `scripts/smoke.sh 30` is the
isolated version with its own socket and state.

### 5. Commit and tag

```sh
git commit -am "release: v0.1.0"
git tag -a v0.1.0 -m "tatami 0.1.0"
git push origin main --tags
```

### 6. Tarball and GitHub release

```sh
V=0.1.0; T=aarch64-apple-darwin; D=tatami-$V-$T
cargo build --release -p tatamid -p tatami
mkdir -p dist/$D/scripts
cp target/release/{tatamid,tatami} dist/$D/
cp scripts/install.sh dist/$D/scripts/
cp README.md CHANGELOG.md LICENSE dist/$D/
cp -r docs dist/$D/
tar -C dist -czf dist/$D.tar.gz $D
shasum -a 256 dist/$D.tar.gz > dist/$D.tar.gz.sha256
gh release create v$V dist/$D.tar.gz dist/$D.tar.gz.sha256 \
    --repo dhishumb/tatami --title "tatami $V" --notes-file <(sed -n "/^## \[$V\]/,/^## \[/p" CHANGELOG.md | sed '$d')
```

The binaries in the tarball are unsigned; `install.sh` signs them on the
user's machine with that machine's identity, which is what the
Accessibility grant needs anyway.

Installing from the tarball:

```sh
gh release download v0.1.0 --repo dhishumb/tatami -p '*.tar.gz'
tar xzf tatami-0.1.0-aarch64-apple-darwin.tar.gz
cd tatami-0.1.0-aarch64-apple-darwin
TATAMI_BIN=. ./scripts/install.sh              # fresh machine
TATAMI_BIN=. ./scripts/install.sh --upgrade    # existing install
```

### 7. Afterwards

Open a fresh `## [Unreleased]` at the top of the CHANGELOG. If the
release changed the installer, run it once more from the tarball on
your own machine; that is the path a new machine takes.
