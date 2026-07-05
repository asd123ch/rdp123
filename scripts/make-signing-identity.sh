#!/usr/bin/env bash
# Create a STABLE self-signed code-signing identity "RDP123 Local" in a DEDICATED
# keychain — fully non-interactive: no login-keychain password or codesign
# access popups. Idempotent: re-running keeps the existing identity.
#
# Why a dedicated keychain: signing from the login keychain triggers a macOS
# password dialog for codesign, and if the login-keychain password is out of sync
# it gets rejected — leaving the build ad-hoc. A dedicated keychain with a random
# password sidesteps all of that without granting arbitrary applications access
# to the private key.
set -euo pipefail
umask 077

IDENTITY="RDP123 Local"
KC="$HOME/Library/Keychains/rdp123-signing.keychain-db"
PW_DIR="$HOME/Library/Application Support/RDP123"
PW_FILE="$PW_DIR/signing-keychain-password"
WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

# Idempotent: already set up? Just make sure it is unlocked and stop.
if [ -f "$KC" ] && [ -f "$PW_FILE" ] &&
   security find-identity -p codesigning "$KC" 2>/dev/null | grep -qF "$IDENTITY"; then
  KCPW="$(cat "$PW_FILE")"
  security unlock-keychain -p "$KCPW" "$KC" 2>/dev/null || true
  echo "Already set up: \"$IDENTITY\" in $KC — nothing to do."
  echo "Build with:  cargo xtask bundle"
  exit 0
fi

echo "Creating self-signed code-signing identity \"$IDENTITY\" in a dedicated keychain..."
KCPW="$(openssl rand -hex 32)"

# 1. Self-signed leaf usable as its own code-signing anchor.
cat > "$WORKDIR/codesign.cnf" <<'EOF'
[ req ]
distinguished_name = dn
x509_extensions    = v3
prompt             = no

[ dn ]
CN = RDP123 Local

[ v3 ]
basicConstraints     = critical, CA:true
keyUsage             = critical, digitalSignature
extendedKeyUsage     = critical, codeSigning
subjectKeyIdentifier = hash
EOF
openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout "$WORKDIR/key.pem" -out "$WORKDIR/cert.pem" \
  -days 3650 -config "$WORKDIR/codesign.cnf" >/dev/null 2>&1

# 2. PKCS#12 bundle. OpenSSL 3.x defaults to a MAC the Security framework can't
#    verify, so force the legacy SHA1/3DES PBE + a non-empty password.
P12PW="$(openssl rand -hex 32)"
openssl pkcs12 -export \
  -inkey "$WORKDIR/key.pem" -in "$WORKDIR/cert.pem" -name "$IDENTITY" \
  -out "$WORKDIR/identity.p12" -passout "pass:${P12PW}" \
  -macalg sha1 -certpbe PBE-SHA1-3DES -keypbe PBE-SHA1-3DES >/dev/null 2>&1

# 3. Fresh dedicated keychain with a random password. Replace legacy setups
# that used a repository-known password or an unrestricted private-key ACL.
security delete-keychain "$KC" 2>/dev/null || true
security create-keychain -p "$KCPW" "$KC"
security set-keychain-settings -lut 21600 "$KC"
security unlock-keychain -p "$KCPW" "$KC"

# 4. Import the key as non-extractable and authorize only the system codesign
# tool. The partition list keeps non-interactive codesign working without the
# insecure `security import -A` (which grants every application access).
security import "$WORKDIR/identity.p12" -k "$KC" -P "$P12PW" -x \
  -T /usr/bin/codesign >/dev/null
security set-key-partition-list -S apple-tool:,apple: -s -k "$KCPW" "$KC" >/dev/null 2>&1

# 5. Verify (no -v: signing does not require the cert to be a trusted anchor, and
#    xtask signs with --keychain "$KC" so there is never any ambiguity).
if security find-identity -p codesigning "$KC" | grep -qF "$IDENTITY"; then
  mkdir -p "$PW_DIR"
  printf '%s\n' "$KCPW" > "$PW_FILE"
  chmod 600 "$PW_FILE"
  echo
  echo "Success: \"$IDENTITY\" is ready — builds sign with no password prompts."
  echo "Build with:  cargo xtask bundle"
else
  echo "ERROR: identity not found after setup." >&2
  exit 1
fi
