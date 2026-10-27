#!/bin/bash
# Cargo runner for `pnpm dev` on macOS (scripts/dev.mjs sets
# CARGO_TARGET_<triple>_RUNNER to this file): signs the freshly built dev
# binary with the local "Varve Dev" identity, then runs it. A dev binary
# is otherwise ad-hoc signed by the linker, which gives it a new identity
# on every rebuild, so the keychain's "Always Allow" never sticks
# (docs/macos-dev-signing.md).
#
# Usage (by cargo): run-signed.sh <binary> [args...]
set -euo pipefail

binary="$1"
shift
identity="${VARVE_DEV_SIGNING_IDENTITY:-Varve Dev}"
# The dev build's own identifier (the release app is com.getvarve.desktop):
# part of the designated requirement the keychain remembers, so the dev
# entry in an item's access list is distinct from the release app's.
# Keychain items are shared by service and account, not by identifier.
identifier="${VARVE_DEV_SIGNING_IDENTIFIER:-com.getvarve.desktop.dev}"

if security find-identity -v -p codesigning 2>/dev/null | grep -qF "\"$identity\""; then
  if ! codesign --force --sign "$identity" --identifier "$identifier" --timestamp=none "$binary" >&2; then
    echo "warning: could not sign $binary with \"$identity\"; running it unsigned" >&2
  fi
else
  echo "note: no \"$identity\" code signing identity; keychain prompts return after every rebuild." >&2
  echo "      One-time setup: scripts/macos/create-dev-signing-identity.sh" >&2
fi

exec "$binary" "$@"
