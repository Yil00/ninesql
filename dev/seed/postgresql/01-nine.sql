-- "nine" sample dataset for pg-wire engines (PostgreSQL, TimescaleDB, YugabyteDB).
-- Fake data only, generated server-side. Dev-only; see dev/README.md.
-- Run inside the database "nine" as the dev user. Idempotent: does nothing if
-- the marker row already exists.

\set ON_ERROR_STOP on

SELECT EXISTS (
  SELECT 1 FROM information_schema.tables
  WHERE table_schema = 'public' AND table_name = 'nine_meta'
) AS nine_seeded \gset

\if :nine_seeded
\echo 'nine dataset already present, skipping'
\quit
\endif

CREATE TABLE customers (
  id          integer PRIMARY KEY,
  name        text        NOT NULL,
  email       text        NOT NULL UNIQUE,
  country     char(2)     NOT NULL,
  created_at  timestamptz NOT NULL
);

CREATE TABLE products (
  id        integer PRIMARY KEY,
  sku       text          NOT NULL UNIQUE,
  name      text          NOT NULL,
  category  text          NOT NULL,
  price     numeric(10,2) NOT NULL
);

CREATE TABLE orders (
  id           integer PRIMARY KEY,
  customer_id  integer     NOT NULL REFERENCES customers (id),
  product_id   integer     NOT NULL REFERENCES products (id),
  quantity     integer     NOT NULL,
  status       text        NOT NULL,
  ordered_at   timestamptz NOT NULL
);
CREATE INDEX orders_customer_idx ON orders (customer_id);

INSERT INTO customers
SELECT i,
       'Customer ' || i,
       'customer' || i || '@example.com',
       (ARRAY['FR','DE','US','JP','BR','IN','NG','CA','AU'])[1 + i % 9],
       timestamptz '2024-01-01 00:00:00+00' + (i * interval '37 minutes')
FROM generate_series(1, 1000) AS i;

INSERT INTO products
SELECT i,
       'SKU-' || lpad(i::text, 5, '0'),
       'Product ' || i,
       (ARRAY['books','games','tools','garden','music'])[1 + i % 5],
       round((1 + (i * 7919) % 50000 / 100.0)::numeric, 2)
FROM generate_series(1, 100) AS i;

INSERT INTO orders
SELECT i,
       1 + (i * 31) % 1000,
       1 + (i * 17) % 100,
       1 + i % 9,
       (ARRAY['new','paid','shipped','delivered','cancelled'])[1 + i % 5],
       timestamptz '2025-01-01 00:00:00+00' + (i * interval '13 minutes')
FROM generate_series(1, 10000) AS i;

CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');

-- One column per commonly used type; row 1 holds typical values, row 2 edge
-- values, row 3 is all NULLs.
CREATE TABLE all_types (
  id               integer PRIMARY KEY,
  c_smallint       smallint,
  c_integer        integer,
  c_bigint         bigint,
  c_numeric        numeric(20,6),
  c_real           real,
  c_double         double precision,
  c_boolean        boolean,
  c_char           char(4),
  c_varchar        varchar(64),
  c_text           text,
  c_bytea          bytea,
  c_date           date,
  c_time           time,
  c_timetz         timetz,
  c_timestamp      timestamp,
  c_timestamptz    timestamptz,
  c_interval       interval,
  c_uuid           uuid,
  c_json           json,
  c_jsonb          jsonb,
  c_xml            xml,
  c_inet           inet,
  c_cidr           cidr,
  c_macaddr        macaddr,
  c_money          money,
  c_int_array      integer[],
  c_text_array     text[],
  c_enum           mood,
  c_point          point,
  c_int4range      int4range,
  c_bit            bit(8),
  c_tsvector       tsvector
);

INSERT INTO all_types VALUES
  (1, 42, 4242, 424242424242, 12345.678901, 3.14, 2.718281828459045, true,
   'abcd', 'varchar value', 'text value with unicode: café, 日本, 🙂', '\xdeadbeef',
   '2026-11-14', '13:37:00', '13:37:00+01', '2026-11-14 13:37:00', '2026-11-14 13:37:00+00',
   '1 day 02:03:04', '6f1c0d2e-9a8b-4c3d-8e7f-1a2b3c4d5e6f',
   '{"a": 1, "b": [true, null]}', '{"nested": {"k": "v"}}', '<nine><v>1</v></nine>',
   '192.0.2.10', '198.51.100.0/24', '08:00:2b:01:02:03', 12.34,
   '{1,2,3}', '{"x","y"}', 'happy', '(1.5,2.5)', '[1,10)', B'10101010',
   to_tsvector('simple', 'nine sample dataset')),
  (2, -32768, -2147483648, 9223372036854775807, -99999999999999.999999, 'NaN', 'Infinity', false,
   '', '', '', '\x', '0001-01-01', '00:00:00', '23:59:59-12', '1970-01-01 00:00:00', '9999-12-31 23:59:59+00',
   '-1 year', '00000000-0000-0000-0000-000000000000',
   '[]', '{}', '<empty/>', '::1', '2001:db8::/32', 'ff:ff:ff:ff:ff:ff', -0.01,
   '{}', '{"", NULL}', 'sad', '(0,0)', 'empty', B'00000000', ''),
  (3, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
   NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
   NULL, NULL, NULL, NULL);

-- 1 000 000 rows, generated server-side.
CREATE TABLE big_events (
  id          bigint PRIMARY KEY,
  event_time  timestamptz      NOT NULL,
  kind        text             NOT NULL,
  value       double precision NOT NULL,
  payload     text
);

INSERT INTO big_events
SELECT i,
       timestamptz '2026-01-01 00:00:00+00' + (i * interval '1 second'),
       (ARRAY['click','view','buy','error'])[1 + i % 4],
       (i % 1000) / 10.0,
       CASE WHEN i % 10 = 0 THEN NULL ELSE 'event ' || i END
FROM generate_series(1, 1000000) AS i;

CREATE VIEW customer_order_totals AS
SELECT c.id, c.name, count(o.id) AS order_count, coalesce(sum(o.quantity * p.price), 0) AS total
FROM customers c
LEFT JOIN orders o ON o.customer_id = c.id
LEFT JOIN products p ON p.id = o.product_id
GROUP BY c.id, c.name;

ANALYZE;

-- Marker last: readiness checks look for it.
CREATE TABLE nine_meta (key text PRIMARY KEY, value text NOT NULL);
INSERT INTO nine_meta VALUES ('seed_version', '1'), ('big_events_rows', '1000000');
