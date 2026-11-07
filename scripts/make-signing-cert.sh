#!/usr/bin/env bash
# Create the self-signed code-signing identity "Murmur Self-Signed" in the login keychain.
# macOS (TCC) recognizes the app by this certificate, so every release must be signed with the
# SAME identity or users lose their microphone and accessibility grants.
# Idempotent: if the identity already exists, nothing changes.
set -euo pipefail

NAME="Murmur Self-Signed"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"
DIR="$HOME/.murmur-keys"
P12="$DIR/murmur-signing.p12"

if security find-identity -p codesigning "$KEYCHAIN" | grep -q "\"$NAME\""; then
  echo "Identity \"$NAME\" already exists in the login keychain. Nothing to do."
  security find-identity -p codesigning "$KEYCHAIN" | grep "\"$NAME\""
  echo
  echo "To export it again for the GitHub secret, open Keychain Access, select the identity,"
  echo "File > Export Items > .p12. Then: base64 -i murmur-signing.p12 | pbcopy"
  exit 0
fi

mkdir -p "$DIR"
chmod 700 "$DIR"
umask 077
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

PASSWORD=$(openssl rand -base64 24 | tr -d '/+=' | cut -c1-24)

cat >"$WORK/openssl.cnf" <<CNF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $NAME
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
CNF

openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -config "$WORK/openssl.cnf" \
  -keyout "$WORK/key.pem" -out "$WORK/cert.pem" 2>/dev/null

# Old-style PKCS12 encryption: the macOS `security` tool cannot read the OpenSSL 3 default.
openssl pkcs12 -export -inkey "$WORK/key.pem" -in "$WORK/cert.pem" -name "$NAME" \
  -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1 \
  -passout "pass:$PASSWORD" -out "$P12"

security import "$P12" -k "$KEYCHAIN" -P "$PASSWORD" -T /usr/bin/codesign >/dev/null

cat >"$DIR/README-signing.txt" <<TXT
Self-signed code-signing identity "$NAME".
PKCS12 file: $P12
Password:    $PASSWORD
GitHub secrets: APPLE_CERTIFICATE = base64 of the .p12, APPLE_CERTIFICATE_PASSWORD = the password,
                APPLE_SIGNING_IDENTITY = $NAME
Never commit these files. Keep the .p12: a new certificate means every user loses their grants.
TXT
chmod 600 "$P12" "$DIR/README-signing.txt"

echo "Created identity \"$NAME\" in the login keychain."
security find-identity -p codesigning "$KEYCHAIN" | grep "\"$NAME\"" || true
echo
echo "For the GitHub secrets (see $DIR/README-signing.txt):"
echo "  base64 -i $P12 | pbcopy      # APPLE_CERTIFICATE"
echo "The identity may show as not trusted; codesign still signs with it by name."
