#!/bin/sh
# Loads the "nine" dataset into Cassandra. Run by the cassandra-seed service:
#   seed.sh <host>
# Fake data only, generated at startup (seq / awk, then cqlsh COPY).
# Idempotent: skips when the marker row exists.
set -eu
HOST="$1"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cql() { cqlsh "$HOST" 9042 "$@"; }

if cql -e "SELECT value FROM nine.nine_meta WHERE key = 'seed_version'" 2>/dev/null | grep -q ' 1$'; then
  echo "nine dataset already present on $HOST, skipping"
  exit 0
fi

cql -f "$(dirname "$0")/nine.cql"

awk 'BEGIN {
  split("FR DE US JP BR IN NG CA AU", c, " ")
  for (i = 1; i <= 1000; i++)
    printf "%d,Customer %d,customer%d@example.com,%s,%d\n", i, i, i, c[1 + i % 9], 1704067200000 + i * 37 * 60000
}' > "$WORK/customers.csv"
awk 'BEGIN {
  split("new paid shipped delivered cancelled", s, " ")
  for (i = 1; i <= 10000; i++)
    printf "%d,%d,%d,%d,%d,%s\n", 1 + (i * 31) % 1000, 1735689600000 + i * 13 * 60000, i, 1 + (i * 17) % 100, 1 + i % 9, s[1 + i % 5]
}' > "$WORK/orders.csv"
awk 'BEGIN {
  split("click view buy error", k, " ")
  for (i = 1; i <= 1000000; i++)
    printf "%d,%d,%s,%.1f,%s\n", i, 1767225600000 + i * 1000, k[1 + i % 4], (i % 1000) / 10, (i % 10 == 0) ? "" : "event " i
}' > "$WORK/big.csv"

# cqlsh exits non-zero when COPY had to retry a timed-out batch, even if every
# row was eventually written: check COPY's summary line instead.
copy() {
  rows="$1"; shift
  out="$(cql -e "$*" 2>&1 || true)"
  echo "$out" | tail -n 1
  echo "$out" | grep -q "^$rows rows imported" || { echo "$out" >&2; exit 1; }
}
copy 1000 "COPY nine.customers (id, name, email, country, created_at) FROM '$WORK/customers.csv'"
copy 10000 "COPY nine.orders_by_customer (customer_id, ordered_at, id, product_id, quantity, status) FROM '$WORK/orders.csv'"
copy 1000000 "COPY nine.big_events (id, event_time, kind, value, payload) FROM '$WORK/big.csv' WITH CHUNKSIZE = 2000 AND MAXBATCHSIZE = 10 AND NUMPROCESSES = 2 AND MAXATTEMPTS = 10"

# Marker last: readiness checks look for it.
cql -e "INSERT INTO nine.nine_meta (key, value) VALUES ('seed_version', '1'); INSERT INTO nine.nine_meta (key, value) VALUES ('big_events_rows', '1000000');"
echo "nine dataset loaded on $HOST"
