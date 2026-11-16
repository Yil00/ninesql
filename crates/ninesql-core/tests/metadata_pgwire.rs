//! Metadata service (P4-B core) against the dev PostgreSQL and its "nine" dataset with the real
//! pgwire driver, as far as the driver's current metadata goes (P4-A extends it).
//!
//! Needs the dev PostgreSQL (`python3 dev/ready.py up core`). Skipped (not failed) when it is
//! unreachable, unless `NINESQL_REQUIRE_ENGINES` lists `postgresql` (AGENTS.md).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)] // Tests may panic (AGENTS.md).

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ninesql_core::driver_api::ObjectKind;
use ninesql_core::driver_api::metadata::ObjectPath;
use ninesql_core::metadata::KindFilter;
use ninesql_core::{
    Auth, Connection, ConnectionFilters, ConnectionTypeId, ConnectionTypes, Endpoint, EngineId,
    MetadataConfig, MetadataError, MetadataService, NodeKey, OpenOutcome, OpenRequest,
    SearchRequest, SecretSlot, SecretSource, TlsMode,
};
use ninesql_driver_pgwire::PgWireDriver;
use ninesql_secrets::SecretString;
use support::*;

struct Dev {
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
}

fn required(engine: &str) -> bool {
    std::env::var("NINESQL_REQUIRE_ENGINES")
        .unwrap_or_default()
        .split([',', ' '])
        .any(|e| e.trim() == engine || e.trim() == "all")
}

fn dev() -> Option<Dev> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dev/engines.json");
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let e = doc["engines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "postgresql")
        .unwrap();
    let dev = Dev {
        host: e["host"].as_str().unwrap().to_owned(),
        port: u16::try_from(e["port"].as_u64().unwrap()).unwrap(),
        user: e["user"].as_str().unwrap().to_owned(),
        password: e["password"].as_str().unwrap().to_owned(),
        database: e["database"].as_str().unwrap().to_owned(),
    };
    let addr = format!("{}:{}", dev.host, dev.port);
    let reachable = addr
        .parse()
        .ok()
        .and_then(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_secs(2)).ok())
        .is_some();
    if reachable {
        return Some(dev);
    }
    assert!(
        !required("postgresql"),
        "postgresql is required but unreachable at {addr}"
    );
    eprintln!("skipped: postgresql unreachable at {addr} (python3 dev/ready.py up core)");
    None
}

fn pg_connection(dev: &Dev, ty: ConnectionTypeId) -> Connection {
    let mut c = Connection::new(
        "Dev PostgreSQL",
        EngineId::new("postgresql").unwrap(),
        Endpoint::Tcp {
            host: dev.host.clone(),
            port: Some(dev.port),
            database: Some(dev.database.clone()),
        },
        Auth::Password {
            username: dev.user.clone(),
            secret: SecretSource::Prompt,
        },
        ty,
    );
    c.tls.mode = TlsMode::Disable;
    c
}

async fn within<F: std::future::Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(30), f)
        .await
        .expect("timed out")
}

#[test]
fn the_nine_dataset_browses_searches_and_completes() {
    let Some(dev) = dev() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(PgWireDriver::new()), tmp.path());
    let svc = MetadataService::new(manager.clone(), MetadataConfig::default());
    let types = ConnectionTypes::default();
    ui(async {
        let connection = pg_connection(&dev, ConnectionTypeId::dev());
        let id = connection.id;
        let request = OpenRequest::new(connection, &types)
            .with_secret(SecretSlot::Auth, SecretString::from(dev.password.clone()));
        let h = match open(&manager, request).await.unwrap() {
            OpenOutcome::Opened(h) => h,
            other => panic!("{other:?}"),
        };

        let root = ObjectPath::root();
        assert_eq!(
            svc.child_kinds(id, &root).unwrap(),
            vec![ObjectKind::Schema]
        );
        let schemas = within(svc.children(id, NodeKey::new(root.clone(), ObjectKind::Schema)))
            .await
            .unwrap();
        let public = schemas
            .items
            .iter()
            .find(|i| i.name() == "public")
            .expect("public schema")
            .path
            .clone();
        let kinds = svc.child_kinds(id, &public).unwrap();
        assert!(kinds.contains(&ObjectKind::Table));
        assert!(kinds.contains(&ObjectKind::View));

        let tables = within(svc.children(id, NodeKey::new(public.clone(), ObjectKind::Table)))
            .await
            .unwrap();
        let names: Vec<&str> = tables.items.iter().map(|i| i.name()).collect();
        for t in [
            "all_types",
            "big_events",
            "customers",
            "nine_meta",
            "orders",
            "products",
        ] {
            assert!(names.contains(&t), "{t} missing from {names:?}");
        }
        assert!(tables.complete);
        let views = within(svc.children(id, NodeKey::new(public.clone(), ObjectKind::View)))
            .await
            .unwrap();
        assert!(
            views
                .items
                .iter()
                .any(|i| i.name() == "customer_order_totals")
        );

        let orders = public.child(ObjectKind::Table, "orders");
        let columns = within(svc.children(id, NodeKey::new(orders.clone(), ObjectKind::Column)))
            .await
            .unwrap();
        let cols: Vec<&str> = columns.items.iter().map(|i| i.name()).collect();
        assert_eq!(
            cols,
            [
                "id",
                "customer_id",
                "product_id",
                "quantity",
                "status",
                "ordered_at"
            ]
        );

        // DDL as the driver declares it (I8): fetched when declared, refused before any call
        // otherwise.
        let ddl = within(svc.ddl(id, orders.clone())).await;
        if svc.supports_ddl(id, ObjectKind::Table) {
            assert!(ddl.unwrap().text.to_lowercase().contains("orders"));
        } else {
            assert_eq!(
                ddl.unwrap_err(),
                MetadataError::DdlUnsupported(Some(ObjectKind::Table))
            );
        }

        // Completion: warm the schema, then read columns from the snapshot.
        let summary = within(svc.warm_schema(id, public.clone())).await.unwrap();
        assert_eq!(summary.failed, 0, "{summary:?}");
        let snap = svc.completion_snapshot(id).unwrap();
        assert!(snap.schemas().iter().any(|s| s == "public"));
        assert!(
            snap.objects(Some("public"))
                .iter()
                .any(|o| o.name == "customers")
        );
        let cols = snap.columns(Some("public"), "customers").unwrap();
        assert_eq!(cols[0].name, "id");
        assert_eq!(cols[0].data_type.as_deref(), Some("integer"));

        // Search over what is cached.
        let r = within(svc.search(SearchRequest::new("ord").in_connections([id])))
            .await
            .unwrap();
        assert_eq!(r.hits[0].name(), "orders");
        let r = within(svc.search(SearchRequest::new("cot").of_kinds([ObjectKind::View])))
            .await
            .unwrap();
        assert_eq!(r.hits[0].name(), "customer_order_totals");

        // Filters: an exclude pattern hides a table; a case-sensitive include is pushed to the
        // server as a bound substring.
        let filters = ConnectionFilters::new()
            .with(
                ObjectKind::Table,
                KindFilter::new(Vec::<&str>::new(), ["big_*"], false).unwrap(),
            )
            .unwrap();
        within(svc.set_filters(id, filters)).await.unwrap().unwrap();
        let tables = within(svc.children(id, NodeKey::new(public.clone(), ObjectKind::Table)))
            .await
            .unwrap();
        assert!(!tables.items.iter().any(|i| i.name() == "big_events"));
        assert_eq!(tables.hidden_by_filter, 1);
        let filters = ConnectionFilters::new()
            .with(
                ObjectKind::Table,
                KindFilter::new(["*ord%rs"], Vec::<&str>::new(), true).unwrap(),
            )
            .unwrap();
        within(svc.set_filters(id, filters)).await.unwrap().unwrap();
        let tables = within(svc.children(id, NodeKey::new(public.clone(), ObjectKind::Table)))
            .await
            .unwrap();
        // `%` is a literal for the server (escaped by the driver): nothing matches.
        assert!(tables.items.is_empty());

        // Closing the session clears the cache.
        within(h.close()).await.unwrap().unwrap();
        within(async {
            while svc.cache_stats(id).is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert_eq!(
            within(svc.children(id, NodeKey::new(root, ObjectKind::Schema)))
                .await
                .unwrap_err(),
            MetadataError::NoSession
        );
    });
}
