#!/bin/sh
# "nine" sample dataset for InfluxDB 3 Core. Fake data only, generated at
# startup with awk as line protocol and written through the CLI. Run by the
# influxdb-seed service; needs INFLUXDB3_HOST_URL and INFLUXDB3_AUTH_TOKEN.
# Idempotent: skips when the nine_meta table exists.
set -eu
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

until curl -fs -H "Authorization: Bearer $INFLUXDB3_AUTH_TOKEN" "$INFLUXDB3_HOST_URL/health" >/dev/null; do sleep 2; done

if influxdb3 query --database nine --format csv "SELECT value FROM nine_meta WHERE key = 'seed_version'" 2>/dev/null | grep -qx 1; then
  echo "nine dataset already present, skipping"
  exit 0
fi

influxdb3 create database nine

# Every field type (float, integer, unsigned, string, boolean) and tags; the
# third point has only one field (InfluxDB stores absent fields as NULL).
cat > "$WORK/all_types.lp" <<'LP'
all_types,id=1,tag_country=FR c_float=2.718281828459045,c_integer=4242i,c_unsigned=18446744073709551615u,c_string="text value with unicode: café, 日本",c_boolean=true 1789393020000000000
all_types,id=2,tag_country=JP c_float=-1e300,c_integer=-9223372036854775808i,c_unsigned=0u,c_string="",c_boolean=false 1789393021000000000
all_types,id=3 c_float=0 1789393022000000000
LP
influxdb3 write --database nine --file "$WORK/all_types.lp"

awk 'BEGIN {
  split("FR DE US JP BR IN NG CA AU", c, " ")
  for (i = 1; i <= 1000; i++)
    printf "customers,country=%s id=%di,name=\"Customer %d\",email=\"customer%d@example.com\" %d000000000\n", c[1 + i % 9], i, i, i, 1704067200 + i * 37 * 60
  split("new paid shipped delivered cancelled", s, " ")
  for (i = 1; i <= 10000; i++)
    printf "orders,status=%s id=%di,customer_id=%di,product_id=%di,quantity=%di %d000000000\n", s[1 + i % 5], i, 1 + (i * 31) % 1000, 1 + (i * 17) % 100, 1 + i % 9, 1735689600 + i * 13 * 60
}' > "$WORK/small.lp"
influxdb3 write --database nine --file "$WORK/small.lp"

# 1 000 000 points in 10 writes of 100 000.
chunk=0
while [ "$chunk" -lt 10 ]; do
  awk -v start="$((chunk * 100000 + 1))" 'BEGIN {
    split("click view buy error", k, " ")
    for (i = start; i < start + 100000; i++) {
      if (i % 10 == 0)
        printf "big_events,kind=%s id=%di,value=%.1f %d000000000\n", k[1 + i % 4], i, (i % 1000) / 10, 1767225600 + i
      else
        printf "big_events,kind=%s id=%di,value=%.1f,payload=\"event %d\" %d000000000\n", k[1 + i % 4], i, (i % 1000) / 10, i, 1767225600 + i
    }
  }' > "$WORK/big.lp"
  influxdb3 write --database nine --file "$WORK/big.lp"
  chunk=$((chunk + 1))
done

# Marker last: readiness checks look for it.
printf 'nine_meta,key=seed_version value="1"\nnine_meta,key=big_events_rows value="1000000"\n' > "$WORK/meta.lp"
influxdb3 write --database nine --file "$WORK/meta.lp"
echo "nine dataset loaded"
