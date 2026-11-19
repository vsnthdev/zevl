#!/usr/bin/env bash
# signs a binary with the local development identity, keeping its code identity
# stable so the macOS keychain does not prompt after every rebuild.

set -euo pipefail

binary="${1:?usage: sign.sh <binary>}"
identity="${CODESIGN_IDENTITY:-zevl-dev}"
keychain="${KEYCHAIN_PATH:-$HOME/Library/Keychains/login.keychain-db}"

if ! security find-identity -p codesigning "$keychain" | grep -q "\"$identity\""; then
    echo "warning: code signing identity '$identity' not found, run scripts/setup-signing.sh (binary left ad-hoc signed)" >&2
    exit 0
fi

codesign --force \
    --identifier "$(basename "$binary")" \
    --sign "$identity" \
    --timestamp=none \
    "$binary" 2>&1 | sed '/replacing existing signature/d'
