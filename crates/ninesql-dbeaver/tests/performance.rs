//! Bounded performance check (P2-A follow-up): a generated 100-connection `data-sources.json`
//! with saved credentials parses and maps well under 2 s. Synthetic content only: hosts under
//! `example.test`, made-up users and passwords.
//!
//! The bound holds in debug builds (what `cargo test` runs); a release build is about ten times
//! faster. The timing covers the whole pipeline of `import_project`: reading both files,
//! decrypting the credentials, parsing, mapping and the file-endpoint checks.

mod common;

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use ninesql_dbeaver::{import_project, open_folder};

const CONNECTIONS: usize = 100;
const BOUND: Duration = Duration::from_secs(2);

/// One connection of a rotating set of shapes: plain PostgreSQL with properties, PostgreSQL
/// through SSH with TLS, MySQL from a JDBC URL with session variables, MariaDB with a TLS
/// handler, SQLite, and an engine outside the v1 set.
fn connection(i: usize) -> String {
    let common = format!(
        r#""name":"perf-{i}","folder":"Team {}","save-password":true"#,
        i % 7
    );
    match i % 6 {
        0 => format!(
            r#""postgres-jdbc-perf-{i}":{{"provider":"postgresql","driver":"postgres-jdbc",{common},
            "configuration":{{"host":"pg{i}.example.test","port":"5432","database":"nine","type":"dev",
            "bootstrap":{{"defaultSchema":"app","query":["SET statement_timeout = 1000"]}},
            "properties":{{"ApplicationName":"perf","currentSchema":"app,public",
            "options":"-c statement_timeout=5000 -c geqo=off"}}}}}}"#
        ),
        1 => format!(
            r#""postgres-jdbc-perf-{i}":{{"provider":"postgresql","driver":"postgres-jdbc",{common},
            "configuration":{{"host":"pg{i}.example.test","port":"5432","database":"nine","type":"prod",
            "handlers":{{"ssh_tunnel":{{"type":"TUNNEL","enabled":true,"save-password":true,
            "properties":{{"host":"bastion{i}.example.test","port":"22","authType":"PASSWORD",
            "jumpServer.count":"1","jumpServer0.enabled":"true","jumpServer0.host":"jump.example.test",
            "jumpServer0.port":"22","jumpServer0.name":"nine_jump","jumpServer0.authType":"AGENT"}}}},
            "postgre_ssl":{{"type":"CONFIG","enabled":true,"properties":{{"sslMode":"verify-full",
            "ssl.ca.cert":"/home/nine_user/certs/ca.pem"}}}}}}}}}}"#
        ),
        2 => format!(
            r#""mysql8-perf-{i}":{{"provider":"mysql","driver":"mysql8",{common},
            "configuration":{{"url":"jdbc:mysql://my{i}.example.test:3306/nine?sslMode=VERIFY_IDENTITY&connectTimeout=10",
            "type":"test","bootstrap":{{"autocommit":false}},
            "properties":{{"sessionVariables":"time_zone='+00:00',sql_mode='ANSI'",
            "useCursorFetch":"true"}}}}}}"#
        ),
        3 => format!(
            r#""mariaDB-perf-{i}":{{"provider":"mysql","driver":"mariaDB",{common},
            "configuration":{{"host":"maria{i}.example.test","port":"3306","database":"nine",
            "handlers":{{"mysql_ssl":{{"type":"CONFIG","enabled":true,"properties":{{"ssl.verify.server":"true",
            "ssl.ca.cert":"/home/nine_user/certs/ca.pem"}}}}}}}}}}"#
        ),
        4 => format!(
            r#""sqlite_jdbc-perf-{i}":{{"provider":"sqlite","driver":"sqlite_jdbc",{common},
            "configuration":{{"database":"/home/nine_user/data/perf-{i}.sqlite",
            "properties":{{"busy_timeout":"2000","foreign_keys":"true"}}}}}}"#
        ),
        _ => format!(
            r#""oracle_thin-perf-{i}":{{"provider":"oracle","driver":"oracle_thin",{common},
            "configuration":{{"host":"ora{i}.example.test","port":"1521","database":"NINE"}}}}"#
        ),
    }
}

#[test]
fn hundred_connections_parse_and_map_under_two_seconds() {
    let mut connections = String::new();
    let mut credentials = String::from("{");
    for i in 0..CONNECTIONS {
        if i > 0 {
            connections.push(',');
            credentials.push(',');
        }
        connections.push_str(&connection(i));
        let id = match i % 6 {
            0 | 1 => format!("postgres-jdbc-perf-{i}"),
            2 => format!("mysql8-perf-{i}"),
            3 => format!("mariaDB-perf-{i}"),
            4 => format!("sqlite_jdbc-perf-{i}"),
            _ => format!("oracle_thin-perf-{i}"),
        };
        let _ = write!(
            credentials,
            r##""{id}":{{"#connection":{{"user":"nine_user","password":"fixture-perf-not-a-secret"}},
            "network/ssh_tunnel":{{"user":"nine_ssh","password":"fixture-perf-ssh-not-a-secret"}}}}"##
        );
    }
    credentials.push('}');
    let folders: String = (0..7)
        .map(|f| format!(r#""Team {f}":{{}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let data_sources = format!(r#"{{"folders":{{{folders}}},"connections":{{{connections}}}}}"#);
    let encrypted = common::encrypt_like_dbeaver(credentials.as_bytes(), [7; 16]);

    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    common::write_project(dir.path(), "General", &data_sources, Some(&encrypted))
        .unwrap_or_else(|e| panic!("{e}"));
    let workspace = open_folder(dir.path()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(workspace.projects.len(), 1);

    let start = Instant::now();
    let p = import_project(&workspace.projects[0]);
    let elapsed = start.elapsed();

    assert_eq!(p.connections.len(), CONNECTIONS);
    assert!(
        p.connections.iter().all(|c| c.imported().is_some()),
        "{:?}",
        p.connections
            .iter()
            .filter(|c| c.imported().is_none())
            .map(|c| &c.issues)
            .collect::<Vec<_>>()
    );
    assert!(p.secret_count() > CONNECTIONS, "{}", p.secret_count());
    assert!(
        elapsed < BOUND,
        "100 connections took {elapsed:?} (bound {BOUND:?})"
    );
}
