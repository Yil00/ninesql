// "nine" sample dataset for Neo4j. Fake data only, generated server-side.
// Run by the neo4j-seed service through cypher-shell; seed.sh skips it when
// the :NineMeta node exists.

CREATE CONSTRAINT customer_id IF NOT EXISTS FOR (c:Customer) REQUIRE c.id IS UNIQUE;
CREATE CONSTRAINT product_id IF NOT EXISTS FOR (p:Product) REQUIRE p.id IS UNIQUE;
CREATE CONSTRAINT order_id IF NOT EXISTS FOR (o:Order) REQUIRE o.id IS UNIQUE;
CREATE INDEX event_kind IF NOT EXISTS FOR (e:Event) ON (e.kind);

UNWIND range(1, 1000) AS i
CREATE (:Customer {id: i, name: 'Customer ' + i, email: 'customer' + i + '@example.com',
  country: ['FR','DE','US','JP','BR','IN','NG','CA','AU'][i % 9],
  created_at: datetime('2024-01-01T00:00:00Z') + duration({minutes: i * 37})});

UNWIND range(1, 100) AS i
CREATE (:Product {id: i, sku: 'SKU-' + i, name: 'Product ' + i,
  category: ['books','games','tools','garden','music'][i % 5],
  price: 1 + (i * 7919) % 50000 / 100.0});

UNWIND range(1, 10000) AS i
MATCH (c:Customer {id: 1 + (i * 31) % 1000}), (p:Product {id: 1 + (i * 17) % 100})
CREATE (c)-[:PLACED]->(o:Order {id: i, status: ['new','paid','shipped','delivered','cancelled'][i % 5],
  ordered_at: datetime('2025-01-01T00:00:00Z') + duration({minutes: i * 13})})-[:CONTAINS {quantity: 1 + i % 9}]->(p);

// One property per Cypher value type; node 3 has no properties besides id.
CREATE (:AllTypes {id: 1, c_integer: 424242424242, c_float: 2.718281828459045, c_string: 'text value with unicode: café, 日本',
  c_boolean: true, c_date: date('2026-11-14'), c_time: time('13:37:00+01:00'), c_localtime: localtime('13:37:00'),
  c_datetime: datetime('2026-11-14T13:37:00Z'), c_localdatetime: localdatetime('2026-11-14T13:37:00'),
  c_duration: duration('P1DT2H3M4S'), c_point: point({longitude: 2.3522, latitude: 48.8566}),
  c_point_cartesian: point({x: 1.5, y: 2.5}), c_list: [1, 2, 3], c_string_list: ['x', 'y']});
CREATE (:AllTypes {id: 2, c_integer: -9223372036854775807, c_float: -1e300, c_string: '', c_boolean: false,
  c_date: date('0001-01-01'), c_duration: duration('PT0S'), c_list: []});
CREATE (:AllTypes {id: 3});

// 1 000 000 nodes, generated server-side in batches.
UNWIND range(1, 1000000) AS i
CALL (i) {
  CREATE (e:Event {id: i, event_time: datetime('2026-01-01T00:00:00Z') + duration({seconds: i}),
    kind: ['click','view','buy','error'][i % 4], value: (i % 1000) / 10.0})
  SET e.payload = CASE WHEN i % 10 = 0 THEN null ELSE 'event ' + i END
} IN TRANSACTIONS OF 50000 ROWS;

// Marker last: readiness checks look for it.
CREATE (:NineMeta {seed_version: '1', events: 1000000});
