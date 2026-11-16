//! Transfer service (P5-C) against the mock driver: a one-million-row export in bounded memory,
//! read-only exports only, file safety (existing file, symbolic link, DBeaver workspace,
//! NineSQL storage), cancel cleanup, and the CSV import (batches, transactions, invalid rows,
//! read-only refusal, production confirmation).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)] // Tests may panic (AGENTS.md).

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ninesql_core::transfer::{
    ColumnMapping, CsvPreviewRequest, ExportFormat, ExportOptions, IssueKind, MappingMode,
    TargetColumn, TransferError,
};
use ninesql_core::{
    ConnectionType, ConnectionTypeId, ConnectionTypes, ExportEvent, ExportRequest, ExportSummary,
    ImportEnd, ImportEvent, ImportRequest, ImportSummary, ImportTransaction, Job, TransferJobError,
    TransferService,
};
use ninesql_storage::{Paths, XdgDirs};
use support::*;

fn paths(root: &Path) -> Paths {
    Paths::new(&XdgDirs {
        config_home: root.join("config"),
        data_home: root.join("data"),
        state_home: root.join("state"),
    })
}

async fn finish_export(mut job: Job<ExportEvent>) -> Result<ExportSummary, TransferJobError> {
    loop {
        match tokio::time::timeout(Duration::from_secs(120), job.next())
            .await
            .expect("export timed out")
        {
            Some(ExportEvent::Done(s)) => return Ok(s),
            Some(ExportEvent::Failed(e)) => return Err(e),
            Some(_) => {}
            None => panic!("export ended without a terminal event"),
        }
    }
}

enum ImportOutcome {
    Confirm(ninesql_core::ImportConfirmation),
    Refused(TransferJobError),
    Done(ImportSummary),
}

async fn finish_import(mut job: Job<ImportEvent>) -> ImportOutcome {
    loop {
        match tokio::time::timeout(Duration::from_secs(60), job.next())
            .await
            .expect("import timed out")
        {
            Some(ImportEvent::Done(s)) => return ImportOutcome::Done(s),
            Some(ImportEvent::Failed(e)) => return ImportOutcome::Refused(e),
            Some(ImportEvent::NeedsConfirmation(c)) => return ImportOutcome::Confirm(c),
            Some(_) => {}
            None => panic!("import ended without a terminal event"),
        }
    }
}

fn done(o: ImportOutcome) -> ImportSummary {
    match o {
        ImportOutcome::Done(s) => s,
        ImportOutcome::Refused(e) => panic!("refused: {e}"),
        ImportOutcome::Confirm(_) => panic!("needs confirmation"),
    }
}

fn dir_entries(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn one_million_rows_export_in_bounded_memory() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), tmp.path());
    let transfer = TransferService::new(manager.clone(), &paths(&tmp.path().join("ninesql")));
    let out = tempfile::tempdir().unwrap();
    let target = out.path().join("million.csv");
    ui(async {
        let h = open_ok(
            &manager,
            mock_connection(ConnectionTypeId::dev()),
            &ConnectionTypes::default(),
        )
        .await;
        let request = ExportRequest::statement(
            "SELECT * FROM numbers(1000000)",
            &target,
            ExportOptions::new(ExportFormat::Csv),
        );
        let budget = request.budget.effective_max_batch_bytes();
        let started = std::time::Instant::now();
        let summary = finish_export(transfer.export(&h, request)).await.unwrap();
        eprintln!(
            "mock 1M-row export: {} rows, {} bytes, {:?}, peak buffered {} bytes (batch budget {budget})",
            summary.rows,
            summary.bytes,
            started.elapsed(),
            summary.peak_buffered_bytes
        );
        assert_eq!(summary.rows, 1_000_000);
        assert!(!summary.incomplete);
        let bound = (ninesql_core::transfer::WRITER_QUEUE as u64 + 2) * budget;
        assert!(summary.peak_buffered_bytes > 0);
        assert!(
            summary.peak_buffered_bytes <= bound,
            "peak {} > bound {bound}",
            summary.peak_buffered_bytes
        );
        assert_eq!(std::fs::metadata(&target).unwrap().len(), summary.bytes);
        let text = std::fs::read_to_string(&target).unwrap();
        assert_eq!(text.lines().count(), 1_000_001);
        assert_eq!(text.lines().next(), Some("id,label"));
        assert_eq!(dir_entries(out.path()), vec!["million.csv"]);
    });
}

#[test]
fn exports_only_reads_and_only_to_safe_new_files() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), tmp.path());
    let ninesql = paths(&tmp.path().join("ninesql"));
    std::fs::create_dir_all(ninesql.data_dir()).unwrap();
    let transfer = TransferService::new(manager.clone(), &ninesql);
    let out = tempfile::tempdir().unwrap();
    let dbeaver = tempfile::tempdir().unwrap();
    manager
        .dbeaver_roots()
        .add(dbeaver.path().canonicalize().unwrap());
    ui(async {
        let h = open_ok(
            &manager,
            mock_connection(ConnectionTypeId::dev()),
            &ConnectionTypes::default(),
        )
        .await;
        let csv = || ExportOptions::new(ExportFormat::Csv);
        // A write is never run by an export.
        let e = finish_export(transfer.export(
            &h,
            ExportRequest::statement(
                "DELETE FROM t WHERE id = 1",
                out.path().join("x.csv"),
                csv(),
            ),
        ))
        .await
        .unwrap_err();
        assert!(matches!(e, TransferJobError::NotARead(_)), "{e}");
        // Existing file: refused, kept; replaced only when confirmed.
        let existing = out.path().join("existing.csv");
        std::fs::write(&existing, "keep").unwrap();
        let e = finish_export(transfer.export(
            &h,
            ExportRequest::statement("SELECT * FROM numbers(3)", &existing, csv()),
        ))
        .await
        .unwrap_err();
        assert!(matches!(
            e,
            TransferJobError::Transfer(TransferError::AlreadyExists(_))
        ));
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "keep");
        let s = finish_export(
            transfer.export(
                &h,
                ExportRequest::statement("SELECT * FROM numbers(3)", &existing, csv())
                    .overwrite_confirmed(),
            ),
        )
        .await
        .unwrap();
        assert_eq!(s.rows, 3);
        // Symbolic link: refused even with the confirmation.
        let victim = out.path().join("victim.txt");
        std::fs::write(&victim, "victim").unwrap();
        let link = out.path().join("link.csv");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        let e = finish_export(
            transfer.export(
                &h,
                ExportRequest::statement("SELECT * FROM numbers(3)", &link, csv())
                    .overwrite_confirmed(),
            ),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            e,
            TransferJobError::Transfer(TransferError::NotARegularFile(_))
        ));
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "victim");
        // DBeaver workspace (I4), also through a symbolic link to its folder.
        let e = finish_export(transfer.export(
            &h,
            ExportRequest::statement(
                "SELECT * FROM numbers(3)",
                dbeaver.path().join("x.csv"),
                csv(),
            ),
        ))
        .await
        .unwrap_err();
        assert!(matches!(
            e,
            TransferJobError::Transfer(TransferError::ForbiddenLocation { .. })
        ));
        let alias = out.path().join("alias");
        std::os::unix::fs::symlink(dbeaver.path(), &alias).unwrap();
        let e = finish_export(transfer.export(
            &h,
            ExportRequest::statement("SELECT * FROM numbers(3)", alias.join("x.csv"), csv()),
        ))
        .await
        .unwrap_err();
        assert!(matches!(
            e,
            TransferJobError::Transfer(TransferError::ForbiddenLocation { .. })
        ));
        assert!(dir_entries(dbeaver.path()).is_empty());
        // NineSQL's own storage (secret vault, workspace).
        let e = finish_export(transfer.export(
            &h,
            ExportRequest::statement(
                "SELECT * FROM numbers(3)",
                ninesql.data_dir().join("vault.csv"),
                csv(),
            ),
        ))
        .await
        .unwrap_err();
        assert!(matches!(
            e,
            TransferJobError::Transfer(TransferError::ForbiddenLocation { .. })
        ));
        // A relative path.
        let e = finish_export(transfer.export(
            &h,
            ExportRequest::statement("SELECT * FROM numbers(3)", "relative.csv", csv()),
        ))
        .await
        .unwrap_err();
        assert!(matches!(
            e,
            TransferJobError::Transfer(TransferError::InvalidPath(_))
        ));
        let mut entries = dir_entries(out.path());
        entries.retain(|e| e.contains("ninesql-part"));
        assert!(entries.is_empty(), "partial files left: {entries:?}");
    });
}

#[test]
fn cancelled_or_failed_exports_leave_no_file() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), tmp.path());
    let transfer = TransferService::new(manager.clone(), &paths(&tmp.path().join("ninesql")));
    let out = tempfile::tempdir().unwrap();
    ui(async {
        let h = open_ok(
            &manager,
            mock_connection(ConnectionTypeId::dev()),
            &ConnectionTypes::default(),
        )
        .await;
        let mut job = transfer.export(
            &h,
            ExportRequest::statement(
                "SELECT * FROM slow_numbers(100000, 20)",
                out.path().join("slow.json"),
                ExportOptions::new(ExportFormat::JsonArray),
            ),
        );
        // Wait until the file exists, then cancel.
        loop {
            match tokio::time::timeout(Duration::from_secs(20), job.next())
                .await
                .unwrap()
            {
                Some(ExportEvent::Started) => break,
                Some(_) => {}
                None => panic!("ended"),
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        job.cancel();
        let e = finish_export(job).await.unwrap_err();
        assert!(matches!(e, TransferJobError::Cancelled), "{e}");
        assert!(
            dir_entries(out.path()).is_empty(),
            "{:?}",
            dir_entries(out.path())
        );
        // An engine error mid-way.
        let e = finish_export(transfer.export(
            &h,
            ExportRequest::statement(
                "SELECT error()",
                out.path().join("err.csv"),
                ExportOptions::new(ExportFormat::Csv),
            ),
        ))
        .await
        .unwrap_err();
        assert!(matches!(e, TransferJobError::Core(_)), "{e}");
        assert!(dir_entries(out.path()).is_empty());
        // The session is still usable.
        let s = finish_export(transfer.export(
            &h,
            ExportRequest::statement(
                "SELECT * FROM numbers(5)",
                out.path().join("ok.md"),
                ExportOptions::new(ExportFormat::Markdown),
            ),
        ))
        .await
        .unwrap();
        assert_eq!(s.rows, 5);
    });
}

fn targets() -> Vec<TargetColumn> {
    vec![
        TargetColumn::new("id", "bigint").with_nullable(false),
        TargetColumn::new("label", "text"),
        TargetColumn::new("born", "date"),
    ]
}

fn write_csv(dir: &Path, rows: usize, bad_line: Option<usize>) -> std::path::PathBuf {
    use std::io::Write;
    let path = dir.join(format!("in-{rows}-{bad_line:?}.csv"));
    let mut f = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    writeln!(f, "id,label,born").unwrap();
    for i in 0..rows {
        if Some(i) == bad_line {
            writeln!(f, "not-a-number,x,2000-01-01").unwrap();
        } else {
            writeln!(f, "{i},\"label, {i}\",2000-01-{:02}", i % 28 + 1).unwrap();
        }
    }
    f.flush().unwrap();
    path
}

#[test]
fn csv_import_batches_transactions_and_invalid_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), tmp.path());
    let transfer = TransferService::new(manager.clone(), &paths(&tmp.path().join("ninesql")));
    let data = tempfile::tempdir().unwrap();
    ui(async {
        let h = open_ok(
            &manager,
            mock_connection(ConnectionTypeId::dev()),
            &ConnectionTypes::default(),
        )
        .await;
        let file = write_csv(data.path(), 1200, None);
        // Preview.
        let p = transfer
            .preview_csv(CsvPreviewRequest::new(&file, targets()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.rows.len(), 100);
        assert!(!p.complete);
        assert!(p.issues.is_empty());
        assert_eq!(p.mapping.columns.len(), 3);
        let mapping = p.mapping.clone();
        let table = vec!["public".to_owned(), "people".to_owned()];
        let s = done(
            finish_import(transfer.import_csv(
                &h,
                ImportRequest::new(&file, table.clone(), targets(), mapping.clone()),
            ))
            .await,
        );
        assert_eq!(s.end, ImportEnd::Completed, "{:?}", s.error);
        assert_eq!(s.rows_read, 1200);
        assert_eq!(s.rows_inserted, 1200);
        assert_eq!(s.rows_committed, 1200);
        assert_eq!(s.statements, 3);
        assert_eq!(s.commits, 1);
        assert!(s.transactional);
        assert!(!h.transaction_state().in_transaction);
        // Commit every 500 rows.
        let s = done(
            finish_import(
                transfer.import_csv(
                    &h,
                    ImportRequest::new(&file, table.clone(), targets(), mapping.clone())
                        .with_transaction(ImportTransaction::CommitEvery(500))
                        .with_rows_per_insert(250),
                ),
            )
            .await,
        );
        assert_eq!((s.statements, s.commits, s.rows_committed), (5, 3, 1200));
        // An invalid row stops an all-or-nothing import: nothing committed, rolled back.
        let bad = write_csv(data.path(), 1200, Some(700));
        let s = done(
            finish_import(transfer.import_csv(
                &h,
                ImportRequest::new(&bad, table.clone(), targets(), mapping.clone()),
            ))
            .await,
        );
        assert_eq!(s.end, ImportEnd::Failed);
        assert!(matches!(s.error, Some(TransferJobError::InvalidRow(_))));
        assert_eq!(s.rows_committed, 0);
        assert_eq!(s.issues.len(), 1);
        assert_eq!(s.issues[0].line, 702);
        assert!(matches!(s.issues[0].kind, IssueKind::Conversion { .. }));
        assert!(!h.transaction_state().in_transaction);
        // Unless invalid rows are skipped.
        let s = done(
            finish_import(
                transfer.import_csv(
                    &h,
                    ImportRequest::new(&bad, table.clone(), targets(), mapping.clone())
                        .skipping_invalid_rows(true),
                ),
            )
            .await,
        );
        assert_eq!(s.end, ImportEnd::Completed);
        assert_eq!(
            (s.rows_inserted, s.rows_skipped, s.issues_total),
            (1199, 1, 1)
        );
        // No value of the file appears in the summary.
        assert!(!format!("{s:?}").contains("label, "));
        // Mapping by position, skipping a column.
        let mut by_pos = ColumnMapping::by_position(3, &targets());
        by_pos.skip_source(2);
        let s = done(
            finish_import(
                transfer.import_csv(&h, ImportRequest::new(&file, table, targets(), by_pos)),
            )
            .await,
        );
        assert_eq!(s.rows_inserted, 1200, "{:?} {:?}", s.end, s.error);
        let _ = MappingMode::ByPosition;
    });
}

#[test]
fn csv_import_refusals_and_production_confirmation() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), tmp.path());
    let ninesql = paths(&tmp.path().join("ninesql"));
    let transfer = TransferService::new(manager.clone(), &ninesql);
    let data = tempfile::tempdir().unwrap();
    let file = write_csv(data.path(), 10, None);
    let table = vec!["people".to_owned()];
    let mapping = ColumnMapping::by_position(3, &targets());
    let mut types = ConnectionTypes::default();
    let mut every = ConnectionType::development();
    every.id = ConnectionTypeId::new("every-write").unwrap();
    every.name = "Every write".into();
    every.confirm_every_write = true;
    types.upsert(every.clone());
    ui(async {
        // Read-only session (I3).
        let mut c = mock_connection(ConnectionTypeId::dev());
        c.read_only = Some(true);
        let ro = open_ok(&manager, c, &types).await;
        match finish_import(transfer.import_csv(
            &ro,
            ImportRequest::new(&file, table.clone(), targets(), mapping.clone()),
        ))
        .await
        {
            ImportOutcome::Refused(TransferJobError::ReadOnlySession) => {}
            _ => panic!("a read-only session must refuse an import"),
        }
        // A non-regular file and a file in NineSQL's storage.
        let dev = open_ok(&manager, mock_connection(ConnectionTypeId::dev()), &types).await;
        match finish_import(transfer.import_csv(
            &dev,
            ImportRequest::new(data.path(), table.clone(), targets(), mapping.clone()),
        ))
        .await
        {
            ImportOutcome::Refused(TransferJobError::Transfer(TransferError::NotARegularFile(
                _,
            ))) => {}
            _ => panic!("a directory is not a CSV file"),
        }
        std::fs::create_dir_all(ninesql.data_dir()).unwrap();
        let inside = ninesql.data_dir().join("x.csv");
        std::fs::copy(&file, &inside).unwrap();
        match finish_import(transfer.import_csv(
            &dev,
            ImportRequest::new(&inside, table.clone(), targets(), mapping.clone()),
        ))
        .await
        {
            ImportOutcome::Refused(TransferJobError::Transfer(
                TransferError::ForbiddenLocation { .. },
            )) => {}
            _ => panic!("NineSQL's storage is refused"),
        }
        // Production: one confirmation for the whole import.
        let prod = open_ok(&manager, mock_connection(ConnectionTypeId::prod()), &types).await;
        let request = ImportRequest::new(&file, table.clone(), targets(), mapping.clone());
        let confirmation = match finish_import(transfer.import_csv(&prod, request.clone())).await {
            ImportOutcome::Confirm(c) => c,
            _ => panic!("production needs a confirmation"),
        };
        assert!(confirmation.production);
        assert_eq!(confirmation.columns, vec!["id", "label", "born"]);
        assert!(confirmation.statement.starts_with("INSERT INTO \"people\""));
        // A token for another request is refused (and spent).
        let other = ImportRequest::new(&file, vec!["other".to_owned()], targets(), mapping.clone())
            .confirmed(confirmation.token);
        match finish_import(transfer.import_csv(&prod, other)).await {
            ImportOutcome::Refused(TransferJobError::InvalidConfirmation) => {}
            _ => panic!("a token is bound to its import"),
        }
        let confirmation = match finish_import(transfer.import_csv(&prod, request.clone())).await {
            ImportOutcome::Confirm(c) => c,
            _ => panic!("production needs a confirmation"),
        };
        let s = done(
            finish_import(
                transfer.import_csv(&prod, request.clone().confirmed(confirmation.token)),
            )
            .await,
        );
        assert_eq!(s.end, ImportEnd::Completed, "{:?}", s.error);
        assert_eq!(s.rows_committed, 10);
        // Single use.
        match finish_import(transfer.import_csv(&prod, request.confirmed(confirmation.token))).await
        {
            ImportOutcome::Refused(TransferJobError::InvalidConfirmation) => {}
            _ => panic!("a token is single-use"),
        }
        // A type that confirms every write: the session's per-statement confirmation is
        // answered for the confirmed INSERT shape.
        let mut c = mock_connection(every.id.clone());
        c.name = "Every".into();
        let ew = open_ok(&manager, c, &types).await;
        let request = ImportRequest::new(&file, table.clone(), targets(), mapping.clone())
            .with_rows_per_insert(3);
        let confirmation = match finish_import(transfer.import_csv(&ew, request.clone())).await {
            ImportOutcome::Confirm(c) => c,
            _ => panic!("confirm-every-write needs a confirmation"),
        };
        assert!(!confirmation.production);
        let s = done(
            finish_import(transfer.import_csv(&ew, request.confirmed(confirmation.token))).await,
        );
        assert_eq!(s.end, ImportEnd::Completed, "{:?}", s.error);
        assert_eq!((s.statements, s.rows_inserted), (4, 10));
    });
}

#[test]
fn review_fixes_lease_file_identity_key_and_protected_aliases() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), tmp.path());
    let ninesql = paths(&tmp.path().join("ninesql"));
    std::fs::create_dir_all(ninesql.data_dir()).unwrap();
    let transfer = TransferService::new(manager.clone(), &ninesql);
    let data = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    ui(async {
        let types = ConnectionTypes::default();
        let h = open_ok(&manager, mock_connection(ConnectionTypeId::dev()), &types).await;
        // L9: while an export holds the session, other statements are refused.
        let mut job = transfer.export(
            &h,
            ExportRequest::statement(
                "SELECT * FROM slow_numbers(100000, 20)",
                out.path().join("slow.csv"),
                ExportOptions::new(ExportFormat::Csv),
            ),
        );
        loop {
            match job.next().await {
                Some(ExportEvent::Started) => break,
                Some(_) => {}
                None => panic!("ended"),
            }
        }
        assert!(h.is_leased());
        match terminal(&run(&h, "SELECT * FROM numbers(1)").await) {
            ninesql_core::ExecEvent::Failed(ninesql_core::CoreError::SessionLeased) => {}
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            h.commit().await,
            Some(Err(ninesql_core::CoreError::SessionLeased))
        ));
        // A second transfer on the same session is refused.
        let file = write_csv(data.path(), 10, None);
        let mapping = ColumnMapping::by_position(3, &targets());
        match finish_import(transfer.import_csv(
            &h,
            ImportRequest::new(&file, vec!["t".into()], targets(), mapping.clone()),
        ))
        .await
        {
            ImportOutcome::Refused(TransferJobError::SessionBusy) => {}
            _ => panic!("a leased session refuses another transfer"),
        }
        job.cancel();
        assert!(matches!(
            finish_export(job).await,
            Err(TransferJobError::Cancelled)
        ));
        assert!(!h.is_leased());
        assert!(matches!(
            terminal(&run(&h, "SELECT * FROM numbers(1)").await),
            ninesql_core::ExecEvent::Finished { .. }
        ));

        // L1 / L2: the token is bound to the file's identity and the import's options.
        let prod = open_ok(&manager, mock_connection(ConnectionTypeId::prod()), &types).await;
        let request = ImportRequest::new(&file, vec!["t".into()], targets(), mapping.clone());
        let token = |o: ImportOutcome| match o {
            ImportOutcome::Confirm(c) => c.token,
            _ => panic!("needs confirmation"),
        };
        let t = token(finish_import(transfer.import_csv(&prod, request.clone())).await);
        let changed = request
            .clone()
            .with_transaction(ImportTransaction::CommitEvery(5))
            .confirmed(t);
        assert!(matches!(
            finish_import(transfer.import_csv(&prod, changed)).await,
            ImportOutcome::Refused(TransferJobError::InvalidConfirmation)
        ));
        let t = token(finish_import(transfer.import_csv(&prod, request.clone())).await);
        // The file is replaced after the confirmation.
        std::fs::write(&file, "id,label,born\n1,x,2000-01-01\n").unwrap();
        assert!(matches!(
            finish_import(transfer.import_csv(&prod, request.clone().confirmed(t))).await,
            ImportOutcome::Refused(TransferJobError::InvalidConfirmation)
        ));

        // M1: an all-invalid file stops at its first issue.
        let mut bad = Vec::from(&b"id,label,born\n"[..]);
        for _ in 0..100_000 {
            bad.extend_from_slice(b"\xff\n");
        }
        let bad_file = data.path().join("bad.csv");
        std::fs::write(&bad_file, bad).unwrap();
        let s = done(
            finish_import(transfer.import_csv(
                &h,
                ImportRequest::new(&bad_file, vec!["t".into()], targets(), mapping.clone()),
            ))
            .await,
        );
        assert_eq!(s.end, ImportEnd::Failed);
        assert_eq!((s.rows_read, s.issues_total), (1, 1));
        let p = transfer
            .preview_csv(CsvPreviewRequest::new(&bad_file, targets()))
            .await
            .unwrap()
            .unwrap();
        assert!(p.rows.is_empty() && !p.complete && p.issues_total == 100);

        // L4: a symbolic link to NineSQL's storage is refused like the storage itself.
        let alias = out.path().join("alias");
        std::os::unix::fs::symlink(ninesql.data_dir(), &alias).unwrap();
        let e = transfer
            .check_export_path(
                &alias.join("x.csv"),
                ninesql_core::transfer::Overwrite::Refuse,
            )
            .unwrap_err();
        assert!(matches!(e, TransferError::ForbiddenLocation { .. }));
    });
}
