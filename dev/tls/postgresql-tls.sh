#!/bin/sh
# Test-only TLS for the dev PostgreSQL service (ADR-0011 conformance tests of the pgwire
# driver). Runs once, at the first start of the container (docker-entrypoint-initdb.d), and
# generates inside the container a throwaway CA and a server certificate for "localhost" and
# 127.0.0.1, then turns `ssl = on`. Plain TCP keeps working: TLS is optional for clients.
# Nothing is committed: no key leaves the container. Tests read the CA certificate (public)
# through SQL: pg_read_file(current_setting('data_directory') || '/ninesql-tls/ca.crt').
# Dev-only, never reused anywhere else.
(
  set -eu
  dir="$PGDATA/ninesql-tls"
  mkdir -p "$dir"
  cd "$dir"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 \
    -subj "/CN=NineSQL dev test CA" -keyout ca.key -out ca.crt \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" \
    2>/dev/null
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -subj "/CN=localhost" -keyout server.key -out server.csr 2>/dev/null
  printf '%s\n' \
    "basicConstraints=critical,CA:FALSE" \
    "keyUsage=critical,digitalSignature" \
    "extendedKeyUsage=serverAuth" \
    "subjectAltName=DNS:localhost,IP:127.0.0.1" > server.ext
  openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 3650 \
    -extfile server.ext -out server.crt 2>/dev/null
  rm -f ca.key ca.srl server.csr server.ext
  chmod 600 server.key
  {
    echo "ssl = on"
    echo "ssl_cert_file = '$dir/server.crt'"
    echo "ssl_key_file = '$dir/server.key'"
  } >> "$PGDATA/postgresql.conf"
  echo "ninesql: test-only TLS enabled (throwaway CA in $dir)"
)
