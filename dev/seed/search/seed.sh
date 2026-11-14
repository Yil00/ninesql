#!/bin/sh
# "nine" sample dataset for Elasticsearch and OpenSearch. Fake data only,
# generated at startup with awk and loaded through the _bulk API (search
# engines have no server-side row generator). Run by the elasticsearch-seed /
# opensearch-seed services:
#   seed.sh <base-url>
# Optional: SEARCH_USER / SEARCH_PASSWORD for basic auth. Idempotent: skips
# when the nine_meta index exists.
set -eu
BASE="$1"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Credentials go through a curl config on stdin, never on the command line.
call() {
  type="$1"; shift
  if [ -n "${SEARCH_USER:-}" ]; then
    printf 'user = "%s:%s"\n' "$SEARCH_USER" "$SEARCH_PASSWORD" | curl -sS -f -K - -H "Content-Type: $type" "$@"
  else
    curl -sS -f -H "Content-Type: $type" "$@"
  fi
}
req() { call application/json "$@"; }
# _bulk answers 200 even when items fail: check the "errors" flag.
bulk() {
  call application/x-ndjson -X POST "$BASE/_bulk?filter_path=errors,items.*.error" --data-binary "@$1" > "$WORK/bulk.out"
  if grep -q '"errors":true' "$WORK/bulk.out"; then head -c 2000 "$WORK/bulk.out" >&2; exit 1; fi
}

if req -o /dev/null -I "$BASE/nine_meta" 2>/dev/null; then
  echo "nine dataset already present at $BASE, skipping"
  exit 0
fi

req -o /dev/null -X PUT "$BASE/customers" -d '{
  "settings": {"number_of_replicas": 0},
  "mappings": {"properties": {
    "name": {"type": "text", "fields": {"raw": {"type": "keyword"}}},
    "email": {"type": "keyword"}, "country": {"type": "keyword"},
    "created_at": {"type": "date"}}}}'
req -o /dev/null -X PUT "$BASE/orders" -d '{
  "settings": {"number_of_replicas": 0},
  "mappings": {"properties": {
    "customer_id": {"type": "integer"}, "product_id": {"type": "integer"},
    "quantity": {"type": "integer"}, "status": {"type": "keyword"},
    "ordered_at": {"type": "date"}}}}'
req -o /dev/null -X PUT "$BASE/all_types" -d '{
  "settings": {"number_of_replicas": 0},
  "mappings": {"properties": {
    "c_keyword": {"type": "keyword"}, "c_text": {"type": "text"},
    "c_long": {"type": "long"}, "c_integer": {"type": "integer"},
    "c_short": {"type": "short"}, "c_byte": {"type": "byte"},
    "c_double": {"type": "double"}, "c_float": {"type": "float"},
    "c_half_float": {"type": "half_float"}, "c_scaled_float": {"type": "scaled_float", "scaling_factor": 100},
    "c_unsigned_long": {"type": "unsigned_long"}, "c_boolean": {"type": "boolean"},
    "c_date": {"type": "date"}, "c_date_nanos": {"type": "date_nanos"},
    "c_binary": {"type": "binary"}, "c_ip": {"type": "ip"},
    "c_geo_point": {"type": "geo_point"}, "c_object": {"type": "object"},
    "c_nested": {"type": "nested"}, "c_integer_range": {"type": "integer_range"}}}}'
req -o /dev/null -X PUT "$BASE/big_events" -d '{
  "settings": {"number_of_replicas": 0, "refresh_interval": "-1"},
  "mappings": {"properties": {
    "event_time": {"type": "date"}, "kind": {"type": "keyword"},
    "value": {"type": "double"}, "payload": {"type": "keyword"}}}}'

awk 'BEGIN {
  split("FR DE US JP BR IN NG CA AU", c, " ")
  for (i = 1; i <= 1000; i++) {
    printf "{\"index\":{\"_index\":\"customers\",\"_id\":\"%d\"}}\n", i
    printf "{\"name\":\"Customer %d\",\"email\":\"customer%d@example.com\",\"country\":\"%s\",\"created_at\":%d}\n", i, i, c[1 + i % 9], 1704067200000 + i * 37 * 60000
  }
  split("new paid shipped delivered cancelled", s, " ")
  for (i = 1; i <= 10000; i++) {
    printf "{\"index\":{\"_index\":\"orders\",\"_id\":\"%d\"}}\n", i
    printf "{\"customer_id\":%d,\"product_id\":%d,\"quantity\":%d,\"status\":\"%s\",\"ordered_at\":%d}\n", 1 + (i * 31) % 1000, 1 + (i * 17) % 100, 1 + i % 9, s[1 + i % 5], 1735689600000 + i * 13 * 60000
  }
}' > "$WORK/small.ndjson"
cat >> "$WORK/small.ndjson" <<'NDJSON'
{"index":{"_index":"all_types","_id":"1"}}
{"c_keyword":"kw","c_text":"text value with unicode: café, 日本","c_long":424242424242,"c_integer":4242,"c_short":42,"c_byte":7,"c_double":2.718281828459045,"c_float":3.14,"c_half_float":1.5,"c_scaled_float":12.34,"c_unsigned_long":18446744073709551615,"c_boolean":true,"c_date":"2026-11-14T13:37:00Z","c_date_nanos":"2026-11-14T13:37:00.123456789Z","c_binary":"3q2+7w==","c_ip":"192.0.2.10","c_geo_point":{"lat":48.8566,"lon":2.3522},"c_object":{"k":"v"},"c_nested":[{"a":1},{"a":2}],"c_integer_range":{"gte":1,"lt":10}}
{"index":{"_index":"all_types","_id":"2"}}
{"c_keyword":"","c_text":"","c_long":-9223372036854775808,"c_integer":-2147483648,"c_short":-32768,"c_byte":-128,"c_double":-1e300,"c_float":-1.5,"c_half_float":0,"c_scaled_float":0,"c_unsigned_long":0,"c_boolean":false,"c_date":"1970-01-01T00:00:00Z","c_date_nanos":"1970-01-01T00:00:00Z","c_binary":"","c_ip":"::1","c_geo_point":{"lat":0,"lon":0},"c_object":{},"c_nested":[],"c_integer_range":{"gte":0,"lte":0}}
{"index":{"_index":"all_types","_id":"3"}}
{"c_keyword":null}
NDJSON
bulk "$WORK/small.ndjson"

# 1 000 000 documents in 20 bulk requests of 50 000.
chunk=0
while [ "$chunk" -lt 20 ]; do
  awk -v start="$((chunk * 50000 + 1))" 'BEGIN {
    split("click view buy error", k, " ")
    for (i = start; i < start + 50000; i++) {
      printf "{\"index\":{\"_index\":\"big_events\",\"_id\":\"%d\"}}\n", i
      if (i % 10 == 0)
        printf "{\"event_time\":%d,\"kind\":\"%s\",\"value\":%.1f}\n", 1767225600000 + i * 1000, k[1 + i % 4], (i % 1000) / 10
      else
        printf "{\"event_time\":%d,\"kind\":\"%s\",\"value\":%.1f,\"payload\":\"event %d\"}\n", 1767225600000 + i * 1000, k[1 + i % 4], (i % 1000) / 10, i
    }
  }' > "$WORK/big.ndjson"
  bulk "$WORK/big.ndjson"
  chunk=$((chunk + 1))
done

req -o /dev/null -X PUT "$BASE/big_events/_settings" -d '{"index": {"refresh_interval": "1s"}}'
req -o /dev/null -X POST "$BASE/_refresh"

# Marker last: readiness checks look for it.
req -o /dev/null -X PUT "$BASE/nine_meta" -d '{"settings": {"number_of_replicas": 0}}'
req -o /dev/null -X PUT "$BASE/nine_meta/_doc/seed_version?refresh=true" -d '{"value": "1"}'
echo "nine dataset loaded at $BASE"
