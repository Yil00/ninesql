-- "nine" sample dataset for DuckDB (file engine: no container). Fake data
-- only, generated with range(). Driver tests apply it to a temporary
-- database file, e.g.  duckdb /tmp/nine.duckdb < dev/seed/duckdb/nine.sql

CREATE TABLE customers (
  id          INTEGER PRIMARY KEY,
  name        VARCHAR NOT NULL,
  email       VARCHAR NOT NULL UNIQUE,
  country     VARCHAR NOT NULL,
  created_at  TIMESTAMPTZ NOT NULL
);

CREATE TABLE products (
  id        INTEGER PRIMARY KEY,
  sku       VARCHAR NOT NULL UNIQUE,
  name      VARCHAR NOT NULL,
  category  VARCHAR NOT NULL,
  price     DECIMAL(10,2) NOT NULL
);

CREATE TABLE orders (
  id           INTEGER PRIMARY KEY,
  customer_id  INTEGER NOT NULL REFERENCES customers (id),
  product_id   INTEGER NOT NULL REFERENCES products (id),
  quantity     INTEGER NOT NULL,
  status       VARCHAR NOT NULL,
  ordered_at   TIMESTAMPTZ NOT NULL
);

INSERT INTO customers
SELECT i, 'Customer ' || i, 'customer' || i || '@example.com',
       ['FR','DE','US','JP','BR','IN','NG','CA','AU'][1 + i % 9],
       TIMESTAMPTZ '2024-01-01 00:00:00+00' + to_minutes(i * 37)
FROM range(1, 1001) t(i);

INSERT INTO products
SELECT i, 'SKU-' || lpad(i::VARCHAR, 5, '0'), 'Product ' || i,
       ['books','games','tools','garden','music'][1 + i % 5],
       round(1 + (i * 7919) % 50000 / 100.0, 2)
FROM range(1, 101) t(i);

INSERT INTO orders
SELECT i, 1 + (i * 31) % 1000, 1 + (i * 17) % 100, 1 + i % 9,
       ['new','paid','shipped','delivered','cancelled'][1 + i % 5],
       TIMESTAMPTZ '2025-01-01 00:00:00+00' + to_minutes(i * 13)
FROM range(1, 10001) t(i);

CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');

-- One column per commonly used type; row 1 typical values, row 2 edge values,
-- row 3 all NULLs.
CREATE TABLE all_types (
  id             INTEGER PRIMARY KEY,
  c_tinyint      TINYINT,
  c_smallint     SMALLINT,
  c_integer      INTEGER,
  c_bigint       BIGINT,
  c_hugeint      HUGEINT,
  c_utinyint     UTINYINT,
  c_ubigint      UBIGINT,
  c_float        FLOAT,
  c_double       DOUBLE,
  c_decimal      DECIMAL(20,6),
  c_boolean      BOOLEAN,
  c_varchar      VARCHAR,
  c_blob         BLOB,
  c_bit          BIT,
  c_date         DATE,
  c_time         TIME,
  c_timestamp    TIMESTAMP,
  c_timestamptz  TIMESTAMPTZ,
  c_interval     INTERVAL,
  c_uuid         UUID,
  c_json         JSON,
  c_enum         mood,
  c_list         INTEGER[],
  c_array        INTEGER[3],
  c_struct       STRUCT(a VARCHAR, b INTEGER),
  c_map          MAP(VARCHAR, INTEGER),
  c_union        UNION(num INTEGER, str VARCHAR)
);

INSERT INTO all_types VALUES
  (1, 7, 42, 4242, 424242424242, 170141183460469231731687303715884105727, 200, 18446744073709551615,
   3.14, 2.718281828459045, 12345.678901, true, 'text value with unicode: café, 日本, 🙂', '\xDE\xAD\xBE\xEF'::BLOB,
   '10101010'::BIT, DATE '2026-11-14', TIME '13:37:00', TIMESTAMP '2026-11-14 13:37:00',
   TIMESTAMPTZ '2026-11-14 13:37:00+00', INTERVAL '1 day 2 hours', '6f1c0d2e-9a8b-4c3d-8e7f-1a2b3c4d5e6f',
   '{"a": 1, "b": [true, null]}', 'happy', [1, 2, 3], [1, 2, 3], {'a': 'x', 'b': 1}, MAP {'k': 1},
   union_value(num := 9)),
  (2, -128, -32768, -2147483648, 9223372036854775807, -170141183460469231731687303715884105727, 0, 0,
   -1.5, -1e300, -99999999999999.999999, false, '', ''::BLOB,
   '0'::BIT, DATE '0001-01-01', TIME '00:00:00', TIMESTAMP '1970-01-01 00:00:00',
   TIMESTAMPTZ '1970-01-01 00:00:00+00', INTERVAL '0 seconds', '00000000-0000-0000-0000-000000000000',
   '[]', 'sad', [], [0, 0, 0], {'a': '', 'b': 0}, MAP {}, union_value(str := '')),
  (3, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
   NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);

-- 1 000 000 rows, generated in the engine.
CREATE TABLE big_events AS
SELECT i AS id,
       TIMESTAMPTZ '2026-01-01 00:00:00+00' + to_seconds(i) AS event_time,
       ['click','view','buy','error'][1 + i % 4] AS kind,
       (i % 1000) / 10.0 AS value,
       CASE WHEN i % 10 = 0 THEN NULL ELSE 'event ' || i END AS payload
FROM range(1, 1000001) t(i);

CREATE VIEW customer_order_totals AS
SELECT c.id, c.name, count(o.id) AS order_count, coalesce(sum(o.quantity * p.price), 0) AS total
FROM customers c
LEFT JOIN orders o ON o.customer_id = c.id
LEFT JOIN products p ON p.id = o.product_id
GROUP BY c.id, c.name;

CREATE TABLE nine_meta (key VARCHAR PRIMARY KEY, value VARCHAR NOT NULL);
INSERT INTO nine_meta VALUES ('seed_version', '1'), ('big_events_rows', '1000000');
