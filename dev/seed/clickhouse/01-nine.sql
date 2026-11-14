-- "nine" sample dataset for ClickHouse. Fake data only, generated server-side
-- with numbers(). Runs once, on first start, from /docker-entrypoint-initdb.d.

CREATE DATABASE IF NOT EXISTS nine;

CREATE TABLE nine.customers (
  id          UInt32,
  name        String,
  email       String,
  country     LowCardinality(String),
  created_at  DateTime('UTC')
) ENGINE = MergeTree ORDER BY id;

INSERT INTO nine.customers
SELECT number + 1, concat('Customer ', toString(number + 1)), concat('customer', toString(number + 1), '@example.com'),
       ['FR','DE','US','JP','BR','IN','NG','CA','AU'][1 + (number + 1) % 9],
       toDateTime('2024-01-01 00:00:00', 'UTC') + toIntervalMinute((number + 1) * 37)
FROM numbers(1000);

CREATE TABLE nine.products (
  id        UInt32,
  sku       String,
  name      String,
  category  LowCardinality(String),
  price     Decimal(10, 2)
) ENGINE = MergeTree ORDER BY id;

INSERT INTO nine.products
SELECT number + 1, concat('SKU-', leftPad(toString(number + 1), 5, '0')), concat('Product ', toString(number + 1)),
       ['books','games','tools','garden','music'][1 + (number + 1) % 5],
       toDecimal64(1 + ((number + 1) * 7919) % 50000 / 100.0, 2)
FROM numbers(100);

CREATE TABLE nine.orders (
  id           UInt32,
  customer_id  UInt32,
  product_id   UInt32,
  quantity     UInt16,
  status       Enum8('new' = 1, 'paid' = 2, 'shipped' = 3, 'delivered' = 4, 'cancelled' = 5),
  ordered_at   DateTime('UTC')
) ENGINE = MergeTree ORDER BY (customer_id, id);

INSERT INTO nine.orders
SELECT n, 1 + (n * 31) % 1000, 1 + (n * 17) % 100, 1 + n % 9, 1 + n % 5,
       toDateTime('2025-01-01 00:00:00', 'UTC') + toIntervalMinute(n * 13)
FROM (SELECT number + 1 AS n FROM numbers(10000));

-- One column per commonly used type; row 1 typical values, row 2 edge values,
-- row 3 NULLs where the type is Nullable (composite types cannot be).
CREATE TABLE nine.all_types (
  id              UInt32,
  c_int8          Nullable(Int8),
  c_int16         Nullable(Int16),
  c_int32         Nullable(Int32),
  c_int64         Nullable(Int64),
  c_int128        Nullable(Int128),
  c_uint8         Nullable(UInt8),
  c_uint64        Nullable(UInt64),
  c_uint256       Nullable(UInt256),
  c_float32       Nullable(Float32),
  c_float64       Nullable(Float64),
  c_decimal       Nullable(Decimal(20, 6)),
  c_bool          Nullable(Bool),
  c_string        Nullable(String),
  c_fixedstring   Nullable(FixedString(4)),
  c_uuid          Nullable(UUID),
  c_date          Nullable(Date),
  c_date32        Nullable(Date32),
  c_datetime      Nullable(DateTime('UTC')),
  c_datetime64    Nullable(DateTime64(6, 'UTC')),
  c_enum          Nullable(Enum8('sad' = 1, 'ok' = 2, 'happy' = 3)),
  c_lowcard       LowCardinality(Nullable(String)),
  c_ipv4          Nullable(IPv4),
  c_ipv6          Nullable(IPv6),
  c_array         Array(Int32),
  c_tuple         Tuple(a String, b Int32),
  c_map           Map(String, UInt64),
  c_json          Nullable(String)
) ENGINE = MergeTree ORDER BY id;

INSERT INTO nine.all_types VALUES
  (1, 7, 42, 4242, 424242424242, 170141183460469231731687303715884105727, 200, 18446744073709551615,
   115792089237316195423570985008687907853269984665640564039457584007913129639935, 3.14, 2.718281828459045,
   12345.678901, true, 'text value with unicode: café, 日本, 🙂', 'abcd', '6f1c0d2e-9a8b-4c3d-8e7f-1a2b3c4d5e6f',
   '2026-11-14', '2026-11-14', '2026-11-14 13:37:00', '2026-11-14 13:37:00.123456', 'happy', 'low',
   '192.0.2.10', '2001:db8::1', [1, 2, 3], ('x', 1), {'k': 1}, '{"a": 1, "b": [true, null]}'),
  (2, -128, -32768, -2147483648, 9223372036854775807, -170141183460469231731687303715884105728, 0, 0,
   0, -1.5, -1e300, -99999999999999.999999, false, '', '\0\0\0\0', '00000000-0000-0000-0000-000000000000',
   '1970-01-01', '1900-01-01', '1970-01-01 00:00:00', '1970-01-01 00:00:00.000000', 'sad', '',
   '0.0.0.0', '::', [], ('', 0), {}, '[]'),
  (3, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
   NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, [], ('', 0), {}, NULL);

-- 1 000 000 rows, generated server-side.
CREATE TABLE nine.big_events (
  id          UInt64,
  event_time  DateTime('UTC'),
  kind        LowCardinality(String),
  value       Float64,
  payload     Nullable(String)
) ENGINE = MergeTree ORDER BY id;

INSERT INTO nine.big_events
SELECT n, toDateTime('2026-01-01 00:00:00', 'UTC') + n, ['click','view','buy','error'][1 + n % 4],
       (n % 1000) / 10.0, if(n % 10 = 0, NULL, concat('event ', toString(n)))
FROM (SELECT number + 1 AS n FROM numbers(1000000));

CREATE VIEW nine.customer_order_totals AS
SELECT c.id, c.name, count(o.id) AS order_count, sum(o.quantity * p.price) AS total
FROM nine.customers AS c
LEFT JOIN nine.orders AS o ON o.customer_id = c.id
LEFT JOIN nine.products AS p ON p.id = o.product_id
GROUP BY c.id, c.name;

-- Marker last: readiness checks look for it.
CREATE TABLE nine.nine_meta (key String, value String) ENGINE = MergeTree ORDER BY key;
INSERT INTO nine.nine_meta VALUES ('seed_version', '1'), ('big_events_rows', '1000000');
