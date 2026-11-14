-- "nine" sample dataset for MySQL and MariaDB. Fake data only, generated
-- server-side (cross joins of a digits table). Runs once, on first start,
-- from /docker-entrypoint-initdb.d in the database "nine".

CREATE TABLE digits (d TINYINT UNSIGNED PRIMARY KEY);
INSERT INTO digits VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9);

CREATE TABLE customers (
  id          INT PRIMARY KEY,
  name        VARCHAR(100) NOT NULL,
  email       VARCHAR(200) NOT NULL UNIQUE,
  country     CHAR(2)      NOT NULL,
  created_at  DATETIME     NOT NULL
) DEFAULT CHARSET = utf8mb4;

CREATE TABLE products (
  id        INT PRIMARY KEY,
  sku       VARCHAR(20)   NOT NULL UNIQUE,
  name      VARCHAR(100)  NOT NULL,
  category  VARCHAR(20)   NOT NULL,
  price     DECIMAL(10,2) NOT NULL
) DEFAULT CHARSET = utf8mb4;

CREATE TABLE orders (
  id           INT PRIMARY KEY,
  customer_id  INT         NOT NULL,
  product_id   INT         NOT NULL,
  quantity     INT         NOT NULL,
  status       VARCHAR(20) NOT NULL,
  ordered_at   DATETIME    NOT NULL,
  KEY orders_customer_idx (customer_id),
  CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES customers (id),
  CONSTRAINT orders_product_fk FOREIGN KEY (product_id) REFERENCES products (id)
) DEFAULT CHARSET = utf8mb4;

INSERT INTO customers
SELECT n, CONCAT('Customer ', n), CONCAT('customer', n, '@example.com'),
       ELT(1 + n % 9, 'FR','DE','US','JP','BR','IN','NG','CA','AU'),
       TIMESTAMPADD(MINUTE, n * 37, '2024-01-01 00:00:00')
FROM (SELECT 1 + a.d + 10 * b.d + 100 * c.d AS n FROM digits a, digits b, digits c) s;

INSERT INTO products
SELECT n, CONCAT('SKU-', LPAD(n, 5, '0')), CONCAT('Product ', n),
       ELT(1 + n % 5, 'books','games','tools','garden','music'),
       ROUND(1 + (n * 7919) % 50000 / 100.0, 2)
FROM (SELECT 1 + a.d + 10 * b.d AS n FROM digits a, digits b) s;

INSERT INTO orders
SELECT n, 1 + (n * 31) % 1000, 1 + (n * 17) % 100, 1 + n % 9,
       ELT(1 + n % 5, 'new','paid','shipped','delivered','cancelled'),
       TIMESTAMPADD(MINUTE, n * 13, '2025-01-01 00:00:00')
FROM (SELECT 1 + a.d + 10 * b.d + 100 * c.d + 1000 * e.d AS n
      FROM digits a, digits b, digits c, digits e) s;

-- One column per commonly used type; row 1 typical values, row 2 edge values,
-- row 3 all NULLs.
CREATE TABLE all_types (
  id             INT PRIMARY KEY,
  c_tinyint      TINYINT,
  c_smallint     SMALLINT,
  c_mediumint    MEDIUMINT,
  c_int          INT,
  c_bigint       BIGINT,
  c_bigint_u     BIGINT UNSIGNED,
  c_decimal      DECIMAL(20,6),
  c_float        FLOAT,
  c_double       DOUBLE,
  c_bit          BIT(8),
  c_boolean      BOOLEAN,
  c_char         CHAR(4),
  c_varchar      VARCHAR(64),
  c_text         TEXT,
  c_mediumtext   MEDIUMTEXT,
  c_binary       BINARY(4),
  c_varbinary    VARBINARY(16),
  c_blob         BLOB,
  c_date         DATE,
  c_time         TIME(6),
  c_datetime     DATETIME(6),
  c_timestamp    TIMESTAMP(6) NULL,
  c_year         YEAR,
  c_enum         ENUM('sad','ok','happy'),
  c_set          SET('a','b','c'),
  c_json         JSON,
  c_geometry     GEOMETRY
) DEFAULT CHARSET = utf8mb4;

INSERT INTO all_types VALUES
  (1, 7, 42, 4242, 424242, 424242424242, 18446744073709551615, 12345.678901, 3.14, 2.718281828459045,
   b'10101010', TRUE, 'abcd', 'varchar value', 'text value with unicode: café, 日本, 🙂', 'medium text',
   X'DEADBEEF', X'CAFE', X'0102030405', '2026-11-14', '13:37:00.123456', '2026-11-14 13:37:00.123456',
   '2026-11-14 13:37:00.123456', 2026, 'happy', 'a,c', '{"a": 1, "b": [true, null]}',
   ST_GeomFromText('POINT(1.5 2.5)')),
  (2, -128, -32768, -8388608, -2147483648, 9223372036854775807, 0, -99999999999999.999999, -1.5, -1e300,
   b'00000000', FALSE, '', '', '', '',
   X'00000000', X'', X'', '1000-01-01', '-838:59:59', '1000-01-01 00:00:00',
   '1970-01-01 00:00:01', 1901, 'sad', '', '[]',
   ST_GeomFromText('LINESTRING(0 0, 1 1)')),
  (3, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
   NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);

-- 1 000 000 rows, generated server-side.
CREATE TABLE big_events (
  id          BIGINT PRIMARY KEY,
  event_time  DATETIME    NOT NULL,
  kind        VARCHAR(8)  NOT NULL,
  value       DOUBLE      NOT NULL,
  payload     VARCHAR(32)
);

INSERT INTO big_events
SELECT n, TIMESTAMPADD(SECOND, n, '2026-01-01 00:00:00'),
       ELT(1 + n % 4, 'click','view','buy','error'),
       (n % 1000) / 10.0,
       IF(n % 10 = 0, NULL, CONCAT('event ', n))
FROM (SELECT 1 + a.d + 10 * b.d + 100 * c.d + 1000 * e.d + 10000 * f.d + 100000 * g.d AS n
      FROM digits a, digits b, digits c, digits e, digits f, digits g) s;

CREATE VIEW customer_order_totals AS
SELECT c.id, c.name, COUNT(o.id) AS order_count, COALESCE(SUM(o.quantity * p.price), 0) AS total
FROM customers c
LEFT JOIN orders o ON o.customer_id = c.id
LEFT JOIN products p ON p.id = o.product_id
GROUP BY c.id, c.name;

-- Marker last: readiness checks look for it.
CREATE TABLE nine_meta (`key` VARCHAR(64) PRIMARY KEY, `value` VARCHAR(255) NOT NULL);
INSERT INTO nine_meta VALUES ('seed_version', '1'), ('big_events_rows', '1000000');
