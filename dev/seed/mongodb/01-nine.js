// "nine" sample dataset for MongoDB. Fake data only, generated server-side by
// mongosh. Runs once, on first start, from /docker-entrypoint-initdb.d
// against MONGO_INITDB_DATABASE ("nine").

const nine = db.getSiblingDB("nine");
const countries = ["FR", "DE", "US", "JP", "BR", "IN", "NG", "CA", "AU"];
const categories = ["books", "games", "tools", "garden", "music"];
const statuses = ["new", "paid", "shipped", "delivered", "cancelled"];
const kinds = ["click", "view", "buy", "error"];

const customers = [];
for (let i = 1; i <= 1000; i++) {
  customers.push({
    _id: i,
    name: `Customer ${i}`,
    email: `customer${i}@example.com`,
    country: countries[i % 9],
    created_at: new Date(Date.UTC(2024, 0, 1) + i * 37 * 60000),
    tags: i % 3 === 0 ? ["vip"] : [],
  });
}
nine.customers.insertMany(customers);
nine.customers.createIndex({ email: 1 }, { unique: true });

const products = [];
for (let i = 1; i <= 100; i++) {
  products.push({
    _id: i,
    sku: `SKU-${String(i).padStart(5, "0")}`,
    name: `Product ${i}`,
    category: categories[i % 5],
    price: NumberDecimal(((1 + ((i * 7919) % 50000) / 100)).toFixed(2)),
  });
}
nine.products.insertMany(products);

const orders = [];
for (let i = 1; i <= 10000; i++) {
  orders.push({
    _id: i,
    customer_id: 1 + ((i * 31) % 1000),
    status: statuses[i % 5],
    ordered_at: new Date(Date.UTC(2025, 0, 1) + i * 13 * 60000),
    lines: [{ product_id: 1 + ((i * 17) % 100), quantity: 1 + (i % 9) }],
  });
}
nine.orders.insertMany(orders);
nine.orders.createIndex({ customer_id: 1 });

// One field per BSON type; document 3 has nulls and missing fields.
nine.all_types.insertMany([
  {
    _id: 1,
    c_double: 2.718281828459045,
    c_string: "text value with unicode: café, 日本, 🙂",
    c_object: { nested: { k: "v" } },
    c_array: [1, "two", { three: 3 }],
    c_binary: BinData(0, "3q2+7w=="),
    c_uuid: UUID("6f1c0d2e-9a8b-4c3d-8e7f-1a2b3c4d5e6f"),
    c_objectid: ObjectId("65a1b2c3d4e5f60718293a4b"),
    c_bool: true,
    c_date: new Date(Date.UTC(2026, 10, 14, 13, 37)),
    c_null: null,
    c_regex: /^nine/i,
    c_javascript: Code("function () { return 9; }"),
    c_int32: NumberInt(4242),
    c_timestamp: Timestamp({ t: 1789393020, i: 1 }),
    c_int64: NumberLong("424242424242"),
    c_decimal128: NumberDecimal("12345.678901"),
    c_minkey: MinKey(),
    c_maxkey: MaxKey(),
  },
  {
    _id: 2,
    c_double: -1e300,
    c_string: "",
    c_object: {},
    c_array: [],
    c_bool: false,
    c_date: new Date(0),
    c_int32: NumberInt(-2147483648),
    c_int64: NumberLong("9223372036854775807"),
    c_decimal128: NumberDecimal("-99999999999999.999999"),
  },
  { _id: 3, c_string: null },
]);

// 1 000 000 documents, generated server-side in batches.
const batch = 10000;
for (let start = 1; start <= 1000000; start += batch) {
  const docs = [];
  for (let i = start; i < start + batch; i++) {
    const doc = {
      _id: i,
      event_time: new Date(Date.UTC(2026, 0, 1) + i * 1000),
      kind: kinds[i % 4],
      value: (i % 1000) / 10,
    };
    if (i % 10 !== 0) doc.payload = `event ${i}`;
    docs.push(doc);
  }
  nine.big_events.insertMany(docs, { ordered: false });
}

// Marker last: readiness checks look for it.
nine.nine_meta.insertMany([
  { _id: "seed_version", value: "1" },
  { _id: "big_events_rows", value: "1000000" },
]);
