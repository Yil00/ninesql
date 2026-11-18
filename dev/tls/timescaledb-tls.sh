#!/bin/sh
# Test-only TLS for the dev TimescaleDB service (ADR-0011 conformance tests of the pgwire
# driver). Runs once, at the first start of the container (docker-entrypoint-initdb.d), and
# turns `ssl = on` with the certificate that the one-shot `timescaledb-tls` service generated
# in /ninesql-tls (tls/generate-certs.sh: the image has no `openssl`). Plain TCP keeps working:
# TLS is optional for clients. Tests read the CA certificate (public) through SQL:
# pg_read_file('/ninesql-tls/ca.crt'). Dev-only, never reused anywhere else.
(
  set -eu
  dir=/ninesql-tls
  for f in ca.crt server.crt server.key; do
    [ -s "$dir/$f" ] || { echo "ninesql: $dir/$f is missing (timescaledb-tls service)" >&2; exit 1; }
  done
  {
    echo "ssl = on"
    echo "ssl_cert_file = '$dir/server.crt'"
    echo "ssl_key_file = '$dir/server.key'"
  } >> "$PGDATA/postgresql.conf"
  echo "ninesql: test-only TLS enabled (throwaway CA in $dir)"
)
