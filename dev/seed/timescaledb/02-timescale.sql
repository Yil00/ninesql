-- TimescaleDB additions to the "nine" dataset: a hypertable and a continuous
-- aggregate. Runs after ../postgresql/01-nine.sql on first start only.

\set ON_ERROR_STOP on

CREATE EXTENSION IF NOT EXISTS timescaledb;

CREATE TABLE sensor_readings (
  sensor_id  integer          NOT NULL,
  time       timestamptz      NOT NULL,
  reading    double precision NOT NULL
);
SELECT create_hypertable('sensor_readings', by_range('time', interval '1 day'));

INSERT INTO sensor_readings
SELECT 1 + i % 20,
       timestamptz '2026-01-01 00:00:00+00' + (i * interval '1 minute'),
       (i % 500) / 5.0
FROM generate_series(1, 100000) AS i;

CREATE MATERIALIZED VIEW sensor_readings_hourly
WITH (timescaledb.continuous) AS
SELECT sensor_id, time_bucket(interval '1 hour', time) AS bucket, avg(reading) AS avg_reading
FROM sensor_readings
GROUP BY sensor_id, bucket
WITH DATA;

INSERT INTO nine_meta VALUES ('timescale_seed_version', '1');
