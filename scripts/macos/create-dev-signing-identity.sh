#!/bin/bash
# One-time macOS setup for `pnpm dev`: creates a self-signed code signing
# certificate (default name "Varve Dev") in your login keychain and trusts
# it for code signing. `scripts/macos/run-signed.sh` then signs every dev
# build with it, so the binary keeps one stable code signature across
# rebuilds and the keychain's "Always Allow" sticks (docs/macos-dev-signing.md).
#
# Usage: scripts/macos/create-dev-signing-identity.sh [name]
#
# macOS asks for your login password once (to add the trust setting) and,
# the first time codesign uses the key, once more (choose Always Allow).
# Nothing here touches the System keychain or system-wide trust settings.
set -euo pipefail

if [ "$(uname -s)" != "Darwin" ]; then
  echo "This script is for macOS only." >&2
  exit 1
fi

name="${1:-Varve Dev}"
keychain="$HOME/Library/Keychains/login.keychain-db"

# The name goes into an OpenSSL config file and into keychain queries:
# letters, digits, spaces, dots, underscores and hyphens only.
if ! [[ "$name" =~ ^[A-Za-z0-9][A-Za-z0-9\ ._-]{0,63}$ ]]; then
  echo "The identity name may only contain letters, digits, spaces, dots, underscores and hyphens (1 to 64 characters)." >&2
  exit 1
fi

if security find-identity -v -p codesigning 2>/dev/null | grep -qF "\"$name\""; then
  echo "A code signing identity named \"$name\" already exists; nothing to do."
  exit 0
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

cat > "$tmp/cert.cnf" <<CNF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $name
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
subjectKeyIdentifier = hash
CNF

# The system LibreSSL: its PKCS#12 output imports into the keychain
# without extra flags, unlike a Homebrew OpenSSL 3.
/usr/bin/openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 3650 \
  -keyout "$tmp/key.pem" -out "$tmp/cert.pem" -config "$tmp/cert.cnf" 2>/dev/null
/usr/bin/openssl pkcs12 -export -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
  -name "$name" -out "$tmp/identity.p12" -passout pass:varve-dev

# -T lets codesign use the key without a prompt for every signature; no
# other tool gets access.
security import "$tmp/identity.p12" -k "$keychain" -P varve-dev \
  -T /usr/bin/codesign >/dev/null
# User (not system) trust for code signing. macOS asks for your password.
security add-trusted-cert -r trustRoot -p codeSign -k "$keychain" "$tmp/cert.pem"

echo "Created the code signing identity:"
security find-identity -v -p codesigning | grep -F "\"$name\""
echo "Run pnpm dev; the first keychain prompt per saved secret is the last one when you choose Always Allow."
