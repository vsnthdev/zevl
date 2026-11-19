#!/usr/bin/env bash
# creates the self-signed code-signing identity used to keep the macOS keychain
# acl valid across rebuilds. run this once per machine.

set -euo pipefail

identity="${CODESIGN_IDENTITY:-zevl-dev}"
keychain="${KEYCHAIN_PATH:-$HOME/Library/Keychains/login.keychain-db}"

if security find-identity -p codesigning "$keychain" | grep -q "\"$identity\""; then
    echo "code signing identity '$identity' already exists"
    exit 0
fi

if ! command -v openssl >/dev/null 2>&1; then
    echo "openssl is required to create the signing identity" >&2
    exit 1
fi

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

password="$(openssl rand -hex 16)"

cat > "$tmp_dir/openssl.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no

[dn]
CN = $identity

[ext]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
EOF

openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes \
    -keyout "$tmp_dir/key.pem" \
    -out "$tmp_dir/cert.pem" \
    -config "$tmp_dir/openssl.cnf" >/dev/null 2>&1

pkcs12_args=(
    -export
    -inkey "$tmp_dir/key.pem"
    -in "$tmp_dir/cert.pem"
    -out "$tmp_dir/identity.p12"
    -passout "pass:$password"
    -name "$identity"
)

# newer openssl versions need legacy algorithms for the macOS importer.
if openssl pkcs12 -help 2>&1 | grep -q -- "-legacy"; then
    pkcs12_args+=(-legacy)
fi

openssl pkcs12 "${pkcs12_args[@]}"

security import "$tmp_dir/identity.p12" \
    -k "$keychain" \
    -P "$password" \
    -T /usr/bin/codesign \
    -T /usr/bin/security >/dev/null

echo "created code signing identity '$identity' in $keychain"
echo "next: run scripts/sign.sh target/debug/zevl"
