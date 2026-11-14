-- "nine" sample dataset for Trino, in the in-memory catalog "memory" (lost on
-- restart: the trino-seed service recreates it on every `up`). Fake data
-- only, generated server-side. The "tpch" catalog is also available.

CREATE SCHEMA memory.nine;

CREATE TABLE memory.nine.customers AS
SELECT CAST(n AS integer) AS id,
       'Customer ' || CAST(n AS varchar) AS name,
       'customer' || CAST(n AS varchar) || '@example.com' AS email,
       element_at(ARRAY['FR','DE','US','JP','BR','IN','NG','CA','AU'], CAST(1 + n % 9 AS integer)) AS country,
       TIMESTAMP '2024-01-01 00:00:00 UTC' + n * INTERVAL '37' MINUTE AS created_at
FROM UNNEST(sequence(1, 1000)) AS t (n);

CREATE TABLE memory.nine.products AS
SELECT CAST(n AS integer) AS id,
       'SKU-' || lpad(CAST(n AS varchar), 5, '0') AS sku,
       'Product ' || CAST(n AS varchar) AS name,
       element_at(ARRAY['books','games','tools','garden','music'], CAST(1 + n % 5 AS integer)) AS category,
       CAST(1 + (n * 7919) % 50000 / 100.0 AS decimal(10, 2)) AS price
FROM UNNEST(sequence(1, 100)) AS t (n);

CREATE TABLE memory.nine.orders AS
SELECT CAST(n AS integer) AS id,
       CAST(1 + (n * 31) % 1000 AS integer) AS customer_id,
       CAST(1 + (n * 17) % 100 AS integer) AS product_id,
       CAST(1 + n % 9 AS integer) AS quantity,
       element_at(ARRAY['new','paid','shipped','delivered','cancelled'], CAST(1 + n % 5 AS integer)) AS status,
       TIMESTAMP '2025-01-01 00:00:00 UTC' + n * INTERVAL '13' MINUTE AS ordered_at
FROM UNNEST(sequence(1, 10000)) AS t (n);

CREATE TABLE memory.nine.all_types (
  id integer,
  c_boolean boolean,
  c_tinyint tinyint,
  c_smallint smallint,
  c_integer integer,
  c_bigint bigint,
  c_real real,
  c_double double,
  c_decimal decimal(20, 6),
  c_varchar varchar,
  c_char char(4),
  c_varbinary varbinary,
  c_json json,
  c_date date,
  c_time time(3),
  c_timestamp timestamp(6),
  c_timestamptz timestamp(6) with time zone,
  c_interval_ds interval day to second,
  c_uuid uuid,
  c_ipaddress ipaddress,
  c_array array(integer),
  c_map map(varchar, integer),
  c_row row(a varchar, b integer)
);

INSERT INTO memory.nine.all_types VALUES
  (1, true, TINYINT '7', SMALLINT '42', 4242, BIGINT '424242424242', REAL '3.14', 2.718281828459045,
   DECIMAL '12345.678901', 'text value with unicode: café, 日本, 🙂', CHAR 'abcd', X'DEADBEEF',
   JSON '{"a": 1, "b": [true, null]}', DATE '2026-11-14', TIME '13:37:00.123',
   TIMESTAMP '2026-11-14 13:37:00.123456', TIMESTAMP '2026-11-14 13:37:00.123456 UTC',
   INTERVAL '1' DAY, UUID '6f1c0d2e-9a8b-4c3d-8e7f-1a2b3c4d5e6f', IPADDRESS '192.0.2.10',
   ARRAY[1, 2, 3], MAP(ARRAY['k'], ARRAY[1]), ROW('x', 1)),
  (2, false, TINYINT '-128', SMALLINT '-32768', -2147483648, BIGINT '9223372036854775807', REAL '-1.5', -1e300,
   DECIMAL '-99999999999999.999999', '', CHAR '', X'', JSON '[]', DATE '1970-01-01', TIME '00:00:00.000',
   TIMESTAMP '1970-01-01 00:00:00.000000', TIMESTAMP '1970-01-01 00:00:00.000000 UTC',
   INTERVAL '0' SECOND, UUID '00000000-0000-0000-0000-000000000000', IPADDRESS '::1',
   ARRAY[], MAP(), ROW('', 0)),
  (3, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
   NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);

-- 1 000 000 rows, generated server-side (sequence() is capped at 10 000
-- elements, hence the cross join).
CREATE TABLE memory.nine.big_events AS
SELECT n AS id,
       TIMESTAMP '2026-01-01 00:00:00 UTC' + n * INTERVAL '1' SECOND AS event_time,
       element_at(ARRAY['click','view','buy','error'], CAST(1 + n % 4 AS integer)) AS kind,
       (n % 1000) / 10.0E0 AS value,
       IF(n % 10 = 0, NULL, 'event ' || CAST(n AS varchar)) AS payload
FROM (SELECT a * 1000 + b + 1 AS n
      FROM UNNEST(sequence(0, 999)) AS x (a)
      CROSS JOIN UNNEST(sequence(0, 999)) AS y (b));

-- Marker last: readiness checks look for it.
CREATE TABLE memory.nine.nine_meta AS
SELECT * FROM (VALUES ('seed_version', '1'), ('big_events_rows', '1000000')) AS t (key, value);
