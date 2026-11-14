#!/bin/sh
# "nine" sample dataset for Redis and Valkey. Fake data only, generated
# server-side with Lua (EVAL). Run by the redis-seed / valkey-seed services:
#   seed.sh <host>
# Requires REDISCLI_AUTH in the environment. Idempotent: skips when the
# marker key exists.
set -eu
HOST="$1"
CLI="${CLI:-redis-cli}"
run() { "$CLI" -h "$HOST" --no-auth-warning "$@"; }

if [ "$(run EXISTS nine:meta)" = "1" ]; then
  echo "nine dataset already present on $HOST, skipping"
  exit 0
fi

# Customers as hashes, products as hashes, orders as a stream, plus one key
# of every core data type.
run EVAL "
for i = 1, 1000 do
  redis.call('HSET', 'nine:customer:' .. i, 'name', 'Customer ' .. i,
    'email', 'customer' .. i .. '@example.com',
    'country', ({'FR','DE','US','JP','BR','IN','NG','CA','AU'})[1 + i % 9])
  redis.call('SADD', 'nine:customers', i)
end
for i = 1, 100 do
  redis.call('HSET', 'nine:product:' .. i, 'sku', string.format('SKU-%05d', i),
    'name', 'Product ' .. i, 'price', string.format('%.2f', 1 + (i * 7919) % 50000 / 100))
  redis.call('ZADD', 'nine:products:by_price', 1 + (i * 7919) % 50000 / 100, i)
end
for i = 1, 10000 do
  redis.call('XADD', 'nine:orders', i .. '-1', 'customer_id', 1 + (i * 31) % 1000,
    'product_id', 1 + (i * 17) % 100, 'quantity', 1 + i % 9)
end
redis.call('SET', 'nine:type:string', 'text value with unicode: café, 日本')
redis.call('SET', 'nine:type:binary', '\\222\\173\\0\\255')
redis.call('SET', 'nine:type:counter', 42)
redis.call('RPUSH', 'nine:type:list', 'a', 'b', 'c')
redis.call('SADD', 'nine:type:set', 'x', 'y', 'z')
redis.call('ZADD', 'nine:type:zset', 1, 'one', 2, 'two', 3, 'three')
redis.call('HSET', 'nine:type:hash', 'field', 'value', 'n', 9)
redis.call('XADD', 'nine:type:stream', '1-1', 'k', 'v')
redis.call('PFADD', 'nine:type:hyperloglog', 'a', 'b', 'c')
redis.call('GEOADD', 'nine:type:geo', 2.3522, 48.8566, 'paris')
redis.call('SETBIT', 'nine:type:bitmap', 9, 1)
redis.call('SET', 'nine:type:expiring', 'ttl', 'EX', 86400)
return 'ok'
" 0

# 1 000 000 string keys, generated server-side in batches of 100 000.
i=0
while [ "$i" -lt 10 ]; do
  run EVAL "
local base = tonumber(ARGV[1])
for i = base + 1, base + 100000 do
  redis.call('SET', 'nine:big:' .. i, 'event ' .. i)
end
return base
" 0 "$((i * 100000))" > /dev/null
  i=$((i + 1))
done

# Marker last: readiness checks look for it.
run HSET nine:meta seed_version 1 big_keys 1000000 > /dev/null
echo "nine dataset loaded on $HOST"
