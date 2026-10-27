# macOS: one keychain prompt, not one per rebuild

Varve keeps passwords and passphrases in the macOS Keychain (`varve` service,
one item per connection and secret kind). The Keychain decides per item which
*application* may read it, and it recognizes the application by its code
signature's designated requirement. When you click "Always Allow" in the
prompt, that requirement is added to the item's access list.

A `pnpm dev` build is only ad-hoc signed by the linker
(`codesign -dv target/debug/varve` shows `Signature=adhoc` and
`flags=...(adhoc,linker-signed)`). An ad-hoc signature has no certificate, so
its requirement is the hash of the binary itself: every rebuild is a new,
unknown application, and the Keychain asks for your login password again.

The fix is a stable, local code signing identity. Release builds are signed
with the real Developer ID later; this identity is only for your machine.

## One-time setup

```sh
scripts/macos/create-dev-signing-identity.sh
```

The script creates a self-signed code signing certificate named "Varve Dev"
in your *login* keychain and trusts it for code signing in your user trust
settings (macOS asks for your login password once for the trust setting). It
never touches the System keychain or system-wide settings. To use another
name, pass it as the first argument and export `VARVE_DEV_SIGNING_IDENTITY`
with the same name before `pnpm dev`.

## What `pnpm dev` does with it

`pnpm dev` runs `scripts/dev.mjs`. On macOS it checks whether the "Varve Dev"
identity exists and, if so, sets `CARGO_TARGET_<triple>_RUNNER` to
`scripts/macos/run-signed.sh` for the `cargo run` that `tauri dev` performs.
Cargo then starts the dev binary through that script, which signs it with
the identity and runs it. The signature is the same for every rebuild, so
the Keychain recognizes the app.

The dev build is signed with the identifier `com.getvarve.desktop.dev`
(override: `VARVE_DEV_SIGNING_IDENTIFIER`), not the release bundle id
`com.getvarve.desktop`. Keychain items are found by service (`varve`) and
account, never by the application's identifier, so dev and release builds
share the same items; the distinct identifier only keeps the dev entry in
an item's access list apart from the release app's (Developer ID) entry.

Without the identity nothing changes: the script prints a hint and the
binary runs ad-hoc signed as before.

## The first prompts after the setup

Keychain items created by earlier (ad-hoc) builds still list the old
requirements, so the first read of each item prompts once more. Choose
**Always Allow**; that prompt is the last one for that item. The first time
`codesign` uses the new key macOS may also ask once ("codesign wants to
sign using key"); choose Always Allow there too.

## What you trade for it

"Always Allow" adds the dev build's designated requirement to the item:
*signed with the "Varve Dev" certificate and identifier
`com.getvarve.desktop.dev`*. The certificate's private key lives in your
login keychain with `codesign` allowed to use it, so any process running
as your user could sign its own binary the same way and then read the
`varve` keychain items without a prompt. That is the same trust you give
every process running as your user anyway (it can also read your source
tree and run `cargo`), and it is limited to this machine and your login
keychain; still, it is one more thing such a process could do. Do not
create the identity on a shared machine. The release app's own access
entry is separate and unaffected.

To remove the identity again:

```sh
pem="$(mktemp)"
security find-certificate -c "Varve Dev" -p > "$pem"
security remove-trusted-cert "$pem"
security delete-identity -c "Varve Dev"
rm -f "$pem"
```

The entries "Always Allow" added to the keychain items stay but match
nothing any more; Keychain Access (item > Get Info > Access Control) shows
and removes them.

## Why it was two prompts per connection, before

Independently of the signature, the app read the same keychain item twice
per connection: once for the user session, and again for the object
browser's metadata session (and once more for every admin, listener or
Pub/Sub session). Each read is one Keychain access, and the Keychain asks
per access until the app is allowed for good. The app now reads every
secret once per run through an in-memory cache (`varve_secrets::SecretCache`)
shared by all sessions of a connection. Settings > Saved secrets has
"Remember secrets for this session" (on by default) and "Forget cached
secrets"; the app menu has "Forget Cached Secrets" as well. The cache is
wiped on quit.
