-- "nine" sample dataset for QuestDB (pg-wire). Fake data only, generated
-- server-side with long_sequence(). Run by the questdb-seed service through
-- psql; the seed script skips it when nine_meta already exists.

CREATE TABLE customers (
  id INT,
  name STRING,
  email STRING,
  country SYMBOL,
  created_at TIMESTAMP
) TIMESTAMP(created_at) PARTITION BY YEAR;

INSERT INTO customers
SELECT x::INT, 'Customer ' || x, 'customer' || x || '@example.com',
       rnd_symbol('FR','DE','US','JP','BR'),
       dateadd('m', (x * 37)::INT, '2024-01-01T00:00:00.000000Z')
FROM long_sequence(1000);

CREATE TABLE products (
  id INT,
  sku STRING,
  name STRING,
  category SYMBOL,
  price DOUBLE
);

INSERT INTO products
SELECT x::INT, 'SKU-' || x, 'Product ' || x,
       rnd_symbol('books','games','tools','garden','music'),
       1 + (x * 7919) % 50000 / 100.0
FROM long_sequence(100);

CREATE TABLE orders (
  id INT,
  customer_id INT,
  product_id INT,
  quantity INT,
  status SYMBOL,
  ordered_at TIMESTAMP
) TIMESTAMP(ordered_at) PARTITION BY MONTH;

INSERT INTO orders
SELECT x::INT, (1 + (x * 31) % 1000)::INT, (1 + (x * 17) % 100)::INT, (1 + x % 9)::INT,
       rnd_symbol('new','paid','shipped','delivered','cancelled'),
       dateadd('m', (x * 13)::INT, '2025-01-01T00:00:00.000000Z')
FROM long_sequence(10000);

-- One column per QuestDB type; row 3 is all NULLs (QuestDB has no NULL for
-- BOOLEAN, BYTE and SHORT: they read back as false / 0).
CREATE TABLE all_types (
  id INT,
  c_boolean BOOLEAN,
  c_byte BYTE,
  c_short SHORT,
  c_char CHAR,
  c_int INT,
  c_long LONG,
  c_float FLOAT,
  c_double DOUBLE,
  c_decimal DECIMAL(18, 4),
  c_string STRING,
  c_varchar VARCHAR,
  c_symbol SYMBOL,
  c_date DATE,
  c_timestamp TIMESTAMP,
  c_timestamp_ns TIMESTAMP_NS,
  c_uuid UUID,
  c_ipv4 IPV4,
  c_long256 LONG256,
  c_geohash GEOHASH(8c),
  c_binary BINARY,
  c_double_array DOUBLE[],
  ts TIMESTAMP
) TIMESTAMP(ts);

INSERT INTO all_types VALUES
  (1, true, 7::BYTE, 42::SHORT, 'n', 4242, 424242424242, 3.14::FLOAT, 2.718281828459045, 12345.6789m,
   'string value', 'varchar café 日本', 'sym', '2026-11-14'::DATE, '2026-11-14T13:37:00.000000Z',
   '2026-11-14T13:37:00.000000001Z'::TIMESTAMP_NS, '6f1c0d2e-9a8b-4c3d-8e7f-1a2b3c4d5e6f', '192.0.2.10',
   CAST('0x01' AS LONG256), #u33dz6vj, NULL, ARRAY[1.0, 2.0, 3.0], '2026-11-14T00:00:00.000000Z'),
  (2, false, -128::BYTE, -32768::SHORT, 'z', -2147483647, 9223372036854775807, -1.5::FLOAT, -1e300, -99999.9999m,
   '', '', 'empty', '1970-01-01'::DATE, '1970-01-01T00:00:00.000000Z',
   '1970-01-01T00:00:00.000000000Z'::TIMESTAMP_NS, '00000000-0000-0000-0000-000000000001', '0.0.0.1',
   CAST('0x00' AS LONG256), #s0000000, NULL, ARRAY[0.0], '2026-11-14T00:00:01.000000Z'),
  (3, false, 0::BYTE, 0::SHORT, NULL, NULL, NULL, NULL, NULL, NULL,
   NULL, NULL, NULL, NULL, NULL,
   NULL, NULL, NULL,
   NULL, NULL, NULL, NULL, '2026-11-14T00:00:02.000000Z');

-- 1 000 000 rows, generated server-side.
CREATE TABLE big_events (
  id LONG,
  event_time TIMESTAMP,
  kind SYMBOL,
  value DOUBLE,
  payload STRING
) TIMESTAMP(event_time) PARTITION BY DAY;

INSERT INTO big_events
SELECT x, dateadd('s', x::INT, '2026-01-01T00:00:00.000000Z'),
       rnd_symbol('click','view','buy','error'),
       (x % 1000) / 10.0,
       CASE WHEN x % 10 = 0 THEN NULL ELSE 'event ' || x END
FROM long_sequence(1000000);

-- Marker last: readiness checks look for it.
CREATE TABLE nine_meta (key STRING, value STRING);
INSERT INTO nine_meta VALUES ('seed_version', '1');
INSERT INTO nine_meta VALUES ('big_events_rows', '1000000');
