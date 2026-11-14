# Test-only TLS for the dev MySQL and MariaDB services (ADR-0011 conformance tests of the
# mysqlwire driver). Sourced once, at the first start of the container
# (docker-entrypoint-initdb.d, after the data directory is initialized and before the seed). It
# generates inside the container a throwaway CA and a server certificate for "localhost" and
# 127.0.0.1 in the data directory, then turns TLS on:
# - MySQL: `SET PERSIST ssl_ca / ssl_cert / ssl_key` and `ALTER INSTANCE RELOAD TLS`;
# - MariaDB: the `--ssl-*` options in compose.yaml point at these files (the first, temporary
#   server starts before they exist and runs without TLS; every later start has them).
# Plain TCP keeps working: TLS is optional for clients. Nothing is committed: no key leaves the
# container. Tests read the CA certificate (public) through SQL: SELECT ca_pem FROM
# nine.ninesql_tls (`tls.ca_sql` in engines.json). Dev-only, never reused anywhere else.
#
# Not executable on purpose: the image entrypoints source it, so `docker_process_sql` (root
# client over the init socket) is available. A subshell keeps `set -e` and `cd` local (no
# `set -u`: the entrypoint helpers read unset positional parameters).
(
  set -e
  dir=/var/lib/mysql/ninesql-tls
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
  ca_hex=$(od -An -v -tx1 ca.crt | tr -d ' \n')
  if command -v mariadbd >/dev/null 2>&1; then
    persist=""
  else
    # Also lets the test user create stored functions with binary logging on (the read-only
    # tests need a function that writes and one that lifts the mode).
    persist="SET PERSIST ssl_ca = '$dir/ca.crt', ssl_cert = '$dir/server.crt', ssl_key = '$dir/server.key'; ALTER INSTANCE RELOAD TLS; SET PERSIST log_bin_trust_function_creators = ON;"
  fi
  docker_process_sql --database=nine <<SQL
CREATE TABLE nine.ninesql_tls (ca_pem TEXT NOT NULL);
INSERT INTO nine.ninesql_tls VALUES (CONVERT(X'$ca_hex' USING utf8mb4));
$persist
SQL
  echo "ninesql: test-only TLS enabled (throwaway CA in $dir)"
)
