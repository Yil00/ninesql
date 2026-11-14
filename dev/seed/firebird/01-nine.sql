-- "nine" sample dataset for Firebird. Fake data only, generated server-side
-- with EXECUTE BLOCK loops. Runs once, on first start, from
-- /docker-entrypoint-initdb.d against the database created by the image
-- (FIREBIRD_DATABASE).

SET TERM ^ ;

CREATE TABLE customers (
  id          INTEGER NOT NULL PRIMARY KEY,
  name        VARCHAR(100) NOT NULL,
  email       VARCHAR(200) NOT NULL UNIQUE,
  country     CHAR(2) NOT NULL,
  created_at  TIMESTAMP WITH TIME ZONE NOT NULL
)^

CREATE TABLE products (
  id        INTEGER NOT NULL PRIMARY KEY,
  sku       VARCHAR(20) NOT NULL UNIQUE,
  name      VARCHAR(100) NOT NULL,
  category  VARCHAR(20) NOT NULL,
  price     DECIMAL(10,2) NOT NULL
)^

CREATE TABLE orders (
  id           INTEGER NOT NULL PRIMARY KEY,
  customer_id  INTEGER NOT NULL REFERENCES customers (id),
  product_id   INTEGER NOT NULL REFERENCES products (id),
  quantity     INTEGER NOT NULL,
  status       VARCHAR(20) NOT NULL,
  ordered_at   TIMESTAMP WITH TIME ZONE NOT NULL
)^

COMMIT^

EXECUTE BLOCK AS
  DECLARE i INTEGER = 1;
BEGIN
  WHILE (i <= 1000) DO
  BEGIN
    INSERT INTO customers VALUES (:i, 'Customer ' || :i, 'customer' || :i || '@example.com',
      DECODE(MOD(:i, 9), 0, 'FR', 1, 'DE', 2, 'US', 3, 'JP', 4, 'BR', 5, 'IN', 6, 'NG', 7, 'CA', 'AU'),
      DATEADD(:i * 37 MINUTE TO TIMESTAMP '2024-01-01 00:00:00 UTC'));
    i = i + 1;
  END
  i = 1;
  WHILE (i <= 100) DO
  BEGIN
    INSERT INTO products VALUES (:i, 'SKU-' || LPAD(:i, 5, '0'), 'Product ' || :i,
      DECODE(MOD(:i, 5), 0, 'books', 1, 'games', 2, 'tools', 3, 'garden', 'music'),
      1 + MOD(:i * 7919, 50000) / 100.0);
    i = i + 1;
  END
  i = 1;
  WHILE (i <= 10000) DO
  BEGIN
    INSERT INTO orders VALUES (:i, 1 + MOD(:i * 31, 1000), 1 + MOD(:i * 17, 100), 1 + MOD(:i, 9),
      DECODE(MOD(:i, 5), 0, 'new', 1, 'paid', 2, 'shipped', 3, 'delivered', 'cancelled'),
      DATEADD(:i * 13 MINUTE TO TIMESTAMP '2025-01-01 00:00:00 UTC'));
    i = i + 1;
  END
END^

COMMIT^

-- One column per commonly used type; row 1 typical values, row 2 edge values,
-- row 3 all NULLs.
CREATE TABLE all_types (
  id               INTEGER NOT NULL PRIMARY KEY,
  c_smallint       SMALLINT,
  c_integer        INTEGER,
  c_bigint         BIGINT,
  c_int128         INT128,
  c_numeric        NUMERIC(18,6),
  c_decimal        DECIMAL(34,6),
  c_decfloat       DECFLOAT(34),
  c_float          FLOAT,
  c_double         DOUBLE PRECISION,
  c_boolean        BOOLEAN,
  c_char           CHAR(4),
  c_varchar        VARCHAR(64),
  c_binary         BINARY(4),
  c_varbinary      VARBINARY(16),
  c_blob_text      BLOB SUB_TYPE TEXT,
  c_blob_binary    BLOB SUB_TYPE BINARY,
  c_date           DATE,
  c_time           TIME,
  c_timetz         TIME WITH TIME ZONE,
  c_timestamp      TIMESTAMP,
  c_timestamptz    TIMESTAMP WITH TIME ZONE
)^

INSERT INTO all_types VALUES (1, 42, 4242, 424242424242, 170141183460469231731687303715884105727,
  12345.678901, 12345.678901, 3.14159265358979323846, 3.14, 2.718281828459045, TRUE,
  'abcd', 'varchar café', X'DEADBEEF', X'CAFE', 'text value with unicode: café, 日本', X'0102030405',
  DATE '2026-11-14', TIME '13:37:00', TIME '13:37:00 UTC', TIMESTAMP '2026-11-14 13:37:00',
  TIMESTAMP '2026-11-14 13:37:00 Europe/Paris')^

INSERT INTO all_types VALUES (2, -32768, -2147483648, 9223372036854775807, -170141183460469231731687303715884105728,
  -999999999999.999999, -999999999999.999999, -1E-300, -1.5, -1E300, FALSE,
  '', '', X'00000000', X'', '', X'',
  DATE '0001-01-01', TIME '00:00:00', TIME '23:59:59 UTC', TIMESTAMP '1970-01-01 00:00:00',
  TIMESTAMP '9999-12-31 23:59:59 UTC')^

INSERT INTO all_types (id) VALUES (3)^

-- 1 000 000 rows, generated server-side.
CREATE TABLE big_events (
  id          BIGINT NOT NULL PRIMARY KEY,
  event_time  TIMESTAMP NOT NULL,
  kind        VARCHAR(8) NOT NULL,
  "VALUE"     DOUBLE PRECISION NOT NULL,
  payload     VARCHAR(32)
)^

COMMIT^

EXECUTE BLOCK AS
  DECLARE i BIGINT = 1;
BEGIN
  WHILE (i <= 1000000) DO
  BEGIN
    INSERT INTO big_events VALUES (:i, DATEADD(:i SECOND TO TIMESTAMP '2026-01-01 00:00:00'),
      DECODE(MOD(:i, 4), 0, 'click', 1, 'view', 2, 'buy', 'error'),
      MOD(:i, 1000) / 10.0,
      IIF(MOD(:i, 10) = 0, NULL, 'event ' || :i));
    i = i + 1;
  END
END^

CREATE VIEW customer_order_totals AS
SELECT c.id, c.name, COUNT(o.id) AS order_count, COALESCE(SUM(o.quantity * p.price), 0) AS total
FROM customers c
LEFT JOIN orders o ON o.customer_id = c.id
LEFT JOIN products p ON p.id = o.product_id
GROUP BY c.id, c.name^

-- Marker last: readiness checks look for it.
CREATE TABLE nine_meta ("KEY" VARCHAR(64) NOT NULL PRIMARY KEY, "VALUE" VARCHAR(255) NOT NULL)^
COMMIT^
INSERT INTO nine_meta VALUES ('seed_version', '1')^
INSERT INTO nine_meta VALUES ('big_events_rows', '1000000')^
COMMIT^

SET TERM ; ^
