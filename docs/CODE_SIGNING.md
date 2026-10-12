# Code signing for the Accessibility grant

macOS keys the Accessibility permission to the **code-signing identity**
of the process that asked. An unsigned (ad-hoc) binary has a new identity
every time it is built, so after every `./scripts/install.sh` macOS would
treat `tatamid` as a new app: the checkbox in System Settings → Privacy &
Security → Accessibility stays ticked but no longer applies, the daemon
runs but manages nothing, and you re-tick it after each upgrade.

The fix is a **stable self-signed identity** in your login keychain. Once
it exists, every install signs `tatamid` and `tatami` with it, the grant
is keyed to that identity, and it survives rebuilds. Same mechanism as
plyterm's `ply-codesign`.

Cost: free. Time: one password prompt, once.

## The identity is created on the first install

`./scripts/install.sh` creates `tatami-codesign` if it is not there:

- a 2048-bit RSA key and self-signed certificate via `openssl req -x509`,
  with the extension shape Apple's keychain accepts for code signing
  (non-critical `keyUsage = digitalSignature`, `extendedKeyUsage =
  codeSigning`, `basicConstraints = CA:FALSE`);
- bundled as PKCS#12 and imported into the login keychain with
  `security import -A -T /usr/bin/codesign`;
- trusted for code signing with `security add-trusted-cert -r trustRoot
  -p codeSign` in the user domain. This is the step that prompts for your
  password (or Touch ID). Accept it.

Then `codesign --force --sign tatami-codesign --identifier
com.vgupta.tatami` on both binaries. The identifier is also the bundle id
embedded in `tatamid` (`crates/tatamid/Info.plist`), so the Accessibility
list shows "tatamid" rather than a path.

After the first signed install macOS asks for Accessibility once more,
because the identity changed from ad-hoc to yours. That is the last time.

## Opting out

```sh
TATAMI_SKIP_CODESIGN=1 ./scripts/install.sh
```

Unsigned install; Accessibility re-prompts after every rebuild. Fine for
a quick try, tedious for daily use.

## Using your own identity

```sh
TATAMI_CODESIGN_CERT="Developer ID Application: You" ./scripts/install.sh
```

Any identity `security find-identity -p codesigning -v` lists. The
installer never auto-creates a cert with a name other than
`tatami-codesign`.

## Checking

```sh
security find-identity -p codesigning -v | grep tatami-codesign
codesign -dv ~/.local/bin/tatamid 2>&1 | grep -E 'Identifier|Authority'
tatami ping                       # "trusted": true
```

## Troubleshooting

- **`codesign` says "no identity found"** right after creation: the trust
  step was declined or failed. Re-run the installer; it retries the
  trust step without recreating the key.
- **Accessibility is ticked but `tatami ping` says `trusted: false`.**
  The ticked entry belongs to an older identity. Remove `tatamid` from
  the list, run `tatami service restart`, and tick the new prompt.
- **The daemon started from a terminal is trusted, the launchd one is
  not.** From a terminal the grant is attributed to the terminal app.
  The launchd agent runs the binary directly and needs its own entry.
- **Starting over:** `security delete-identity -c tatami-codesign`, then
  `./scripts/install.sh` creates it again (and macOS prompts for
  Accessibility again, once).

Nothing here leaves your machine; the certificate is self-signed and
lives only in your login keychain.
