#!/bin/sh
# Test-only TLS certificates for dev services whose image has no `openssl` (ADR-0011
# conformance tests): run by a one-shot "<engine>-tls" service of compose.yaml on an image that
# has it, into a volume shared with the engine, or by the engine's own start command.
# Usage: generate-certs.sh <dir> <uid> [<gid> [<name>]].
#
# Generates in <dir> a throwaway CA (ca.crt) and a server certificate for "localhost" and
# 127.0.0.1 (<name>.crt and <name>.key, <name> "server" by default), the key owned by
# <uid>:<gid> (the engine's server user) with mode 0600. Does nothing when the files
# already exist, so a later `up` keeps the certificates the engine was configured with. The CA
# key is deleted; no key is committed or leaves the Docker volume. Tests read the CA (public)
# through SQL (`tls.ca_sql` in engines.json). Dev-only, never reused anywhere else.
set -eu
dir="$1"
uid="$2"
gid="${3:-$2}"
name="${4:-server}"
if [ -s "$dir/ca.crt" ] && [ -s "$dir/$name.crt" ] && [ -s "$dir/$name.key" ]; then
  echo "ninesql: test-only TLS certificates already present in $dir"
  exit 0
fi
mkdir -p "$dir"
cd "$dir"
rm -f ca.crt "$name.crt" "$name.key"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 \
  -subj "/CN=NineSQL dev test CA" -keyout ca.key -out ca.crt \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" \
  2>/dev/null
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -subj "/CN=localhost" -keyout "$name.key" -out server.csr 2>/dev/null
printf '%s\n' \
  "basicConstraints=critical,CA:FALSE" \
  "keyUsage=critical,digitalSignature" \
  "extendedKeyUsage=serverAuth" \
  "subjectAltName=DNS:localhost,IP:127.0.0.1" > server.ext
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 3650 \
  -extfile server.ext -out "$name.crt" 2>/dev/null
rm -f ca.key ca.srl server.csr server.ext
chmod 644 ca.crt "$name.crt"
chown "$uid:$gid" "$name.key"
chmod 600 "$name.key"
echo "ninesql: test-only TLS certificates generated in $dir (throwaway CA)"
