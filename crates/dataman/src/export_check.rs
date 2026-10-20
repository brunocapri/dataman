//! `export-check`: ⌘E on a result, each format picked with a click and written to the default
//! export folder, the files read back. `import-check`: CSV, TSV, JSON, NDJSON and Parquet files
//! imported into new and existing PostgreSQL tables (one dropped onto the window), Latin-1 text,
//! bad rows skipped, a truncate behind the guard. (Import Data is PostgreSQL's only.)

use std::path::{Path, PathBuf};

use gpui::{AsyncWindowContext, Entity, ExternalPaths, FileDropEvent, Focusable, PlatformInput, point};
use dataman_core::config::Engine;

use crate::app::Workspace;
use crate::check::*;
use crate::data_import::{DataImportView, DataTarget};

const SQL: &str = "SELECT id, sku, name, price, stock, active FROM dataman_scratch.products ORDER BY id";

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dataman-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub async fn run_export(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let (pg, pw) = profile(Engine::Postgres);
    let up = connect(ws, &pg, &pw, cx).await;
    if !r.check("PostgreSQL connects", up.is_ok(), format!("{up:?}")) {
        return;
    }
    let dir = scratch("export");
    let core = core(ws, cx);
    let _ = core.settings.update(|s| s.export_dir = Some(dir.display().to_string()));
    let Some(tab) = run_keys(ws, SQL, "cmd-enter", cx).await else { return };
    let rows = result(&tab, cx).1;
    r.check("the result to export: 10 products", rows == 10, rows);
    let grid = cx.update(|_, cx| tab.read(cx).grid.clone()).unwrap();
    for (label, ext) in [("CSV", "csv"), ("TSV", "tsv"), ("JSON", "json"), ("NDJSON", "ndjson"), ("SQL", "sql"), ("Parquet", "parquet")] {
        let _ = cx.update(|window, cx| window.focus(&grid.focus_handle(cx), cx));
        key(cx, "cmd-e");
        pause(cx, 150).await;
        let Some(view) = cx.update(|_, cx| ws.read(cx).export.clone()).ok().flatten() else {
            r.check(&format!("⌘E opens the export sheet ({label})"), false, "no sheet");
            continue;
        };
        click_probe(&format!("ex-fmt-{label}"), cx);
        click_probe("ex-start", cx);
        until(cx, 20, |cx| cx.update(|_, cx| view.read(cx).demo_outcome().is_some()).unwrap_or(false)).await;
        let out = cx.update(|_, cx| view.read(cx).demo_outcome()).ok().flatten();
        let file = match &out {
            Some(Ok((p, n))) if p.parent() == Some(dir.as_path()) && p.extension().and_then(|e| e.to_str()) == Some(ext) && *n == 10 => Some(p.clone()),
            _ => None,
        };
        let verdict = file.as_deref().map(|p| contents(p, label)).unwrap_or_else(|| Err(format!("{out:?}")));
        r.check(&format!("{label}: 10 rows into the default export folder, the file reads back"), verdict.is_ok(), verdict.err().unwrap_or_default());
        key(cx, "escape");
        pause(cx, 100).await;
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn contents(p: &Path, label: &str) -> Result<(), String> {
    let bytes = std::fs::read(p).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let ok = match label {
        "CSV" => lines.first() == Some(&"id,sku,name,price,stock,active") && lines.len() == 11 && lines[1].starts_with("101,AUD-0101,"),
        "TSV" => lines.first() == Some(&"id\tsku\tname\tprice\tstock\tactive") && lines.len() == 11,
        "JSON" => serde_json::from_slice::<serde_json::Value>(&bytes).ok().and_then(|v| v.as_array().map(|a| a.len() == 10 && a[0]["sku"] == "AUD-0101")).unwrap_or(false),
        "NDJSON" => lines.len() == 10 && lines.iter().all(|l| serde_json::from_str::<serde_json::Value>(l).is_ok()),
        "SQL" => {
            let stmts: Vec<&str> = dataman_core::sql::split(&text).into_iter().map(|s| text[s.range].trim()).filter(|s| !s.is_empty() && !s.starts_with("--")).collect();
            !stmts.is_empty() && stmts.iter().all(|s| s.starts_with("INSERT INTO")) && text.matches("'AUD-01").count() == 10
        }
        _ => {
            let pv = dataman_core::import::preview(p, None, None).map_err(|e| format!("{e:#}"))?;
            bytes.starts_with(b"PAR1") && pv.total_rows == Some(10) && pv.columns.len() == 6 && pv.rows.first().and_then(|r| r[1].clone()).as_deref() == Some("AUD-0101")
        }
    };
    if ok { Ok(()) } else { Err(text.chars().take(300).collect()) }
}

fn target(ws: &Entity<Workspace>, conn: &str, db: &str, schema: &str, table: Option<&str>, cx: &mut AsyncWindowContext) -> Option<DataTarget> {
    let c = cx.update(|_, cx| ws.read(cx).store.read(cx).connection(conn)).ok().flatten()?;
    Some(DataTarget { conn_id: conn.into(), conn: c, db: db.into(), schema: schema.into(), table: table.map(str::to_string) })
}

fn view(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Entity<DataImportView>> {
    cx.update(|_, cx| ws.read(cx).data_import.clone()).ok().flatten()
}

/// Waits for the sheet's preview, sets its options, ⌘↩ (again past the guard's question) and
/// waits for the report.
async fn import(ws: &Entity<Workspace>, truncate: bool, skip: bool, cx: &mut AsyncWindowContext) -> Option<(u64, u64, bool)> {
    let v = view(ws, cx)?;
    until(cx, 15, |cx| cx.update(|_, cx| v.read(cx).demo_ready()).unwrap_or(false)).await;
    let _ = cx.update(|window, cx| {
        v.update(cx, |v, _| {
            v.demo_set_truncate(truncate);
            v.demo_set_skip(skip);
        });
        window.focus(&v.focus_handle(cx), cx);
    });
    key(cx, "cmd-enter");
    pause(cx, 200).await;
    if !cx.update(|_, cx| v.read(cx).is_running() || v.read(cx).demo_report().is_some()).unwrap_or(true) {
        key(cx, "cmd-enter");
    }
    until(cx, 30, |cx| cx.update(|_, cx| v.read(cx).demo_report().is_some()).unwrap_or(false)).await;
    let rep = cx.update(|_, cx| v.read(cx).demo_report()).ok().flatten();
    key(cx, "escape");
    pause(cx, 100).await;
    rep
}

pub async fn run_import(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let dir = scratch("import");
    let rows: Vec<(i32, &str, f64)> = (1..=20).map(|i| (i, ["ana", "bo", "cy", "dee"][i as usize % 4], i as f64 * 1.5)).collect();
    let csv: String = std::iter::once("id,name,amount\n".to_string()).chain(rows.iter().map(|(i, n, a)| format!("{i},{n},{a}\n"))).collect();
    std::fs::write(dir.join("e2e_csv.csv"), &csv).unwrap();
    std::fs::write(dir.join("e2e_tsv.tsv"), csv.replace(',', "\t")).unwrap();
    let json: Vec<serde_json::Value> = rows.iter().map(|(i, n, a)| serde_json::json!({"id": i, "name": n, "amount": a})).collect();
    std::fs::write(dir.join("e2e_json.json"), serde_json::to_vec(&json).unwrap()).unwrap();
    std::fs::write(dir.join("e2e_ndjson.ndjson"), json.iter().map(|v| format!("{v}\n")).collect::<String>()).unwrap();
    let mut latin = b"id,name\n1,".to_vec();
    latin.extend_from_slice(&[b'S', 0xE3, b'o', b' ', b'P', b'a', b'u', b'l', b'o', b'\n']);
    std::fs::write(dir.join("e2e_latin.csv"), latin).unwrap();
    std::fs::write(dir.join("e2e_bad.csv"), "id,amount\n1,1.5\n2,not a number\n3,4.5\n").unwrap();

    let (pg, pw) = profile(Engine::Postgres);
    let up = connect(ws, &pg, &pw, cx).await;
    if !r.check("PostgreSQL connects", up.is_ok(), format!("{up:?}")) {
        return;
    }
    let core = core(ws, cx);
    let _ = sql(&core, &pg, &pw, "DROP SCHEMA IF EXISTS e2e_import CASCADE", cx).await;
    let _ = sql(&core, &pg, &pw, "CREATE SCHEMA e2e_import", cx).await;

    for file in ["e2e_csv.csv", "e2e_tsv.tsv", "e2e_json.json", "e2e_ndjson.ndjson"] {
        let table = file.split('.').next().unwrap();
        let t = target(ws, &pg.id, &pg.database, "e2e_import", None, cx);
        let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.import_data(t, Some(dir.join(file)), window, cx)));
        let rep = import(ws, false, false, cx).await;
        let got = sql(&core, &pg, &pw, &format!("SELECT count(*), sum(amount)::float8::text, max(name) FROM e2e_import.{table}"), cx).await;
        r.check(
            &format!("PostgreSQL: {file} into a new table, every row and value"),
            rep.is_some_and(|r| r.0 == 20) && got.as_ref().ok().and_then(|g| g.first().cloned()) == Some(vec![Some("20".into()), Some("315".into()), Some("dee".into())]),
            format!("{rep:?} {got:?}"),
        );
    }

    let parquet = dir.join("e2e_parquet.parquet");
    let exported = {
        let c = cx.update(|_, cx| ws.read(cx).store.read(cx).connection(&pg.id)).ok().flatten();
        match c {
            Some(c) => {
                let mut opts = dataman_core::export::ExportOptions::new(dataman_core::export::ExportFormat::Parquet);
                opts.table = "e2e_parquet".into();
                let e = c.export_query(Some(&pg.database), "SELECT id, name, amount FROM e2e_import.e2e_csv ORDER BY id", parquet.clone(), opts, None);
                e.task.await.is_ok()
            }
            None => false,
        }
    };
    let window_size = cx.update(|window, _| window.viewport_size()).unwrap();
    let tree = cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap();
    let schema_row = format!("c:{}/d:{}/s:e2e_import", pg.id, pg.database);
    let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.refresh_connection(&pg.id, cx)));
    until(cx, 10, |cx| {
        let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.expand_id(&format!("c:{}/d:{}", pg.id, pg.database), cx)));
        cx.update(|_, cx| tree.read(cx).has_row(&schema_row)).unwrap_or(false)
    })
    .await;
    let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.select_id(&schema_row, cx)));
    // Onto the editor, the grid and the explorer: wherever it lands in the window.
    let mut missed = Vec::new();
    for (name, fx, fy) in [("editor", 0.6, 0.3), ("grid", 0.6, 0.75), ("explorer", 0.08, 0.5)] {
        let at = point(window_size.width * fx, window_size.height * fy);
        input(cx, PlatformInput::FileDrop(FileDropEvent::Entered { position: at, paths: ExternalPaths(vec![parquet.clone()].into()) }));
        input(cx, PlatformInput::FileDrop(FileDropEvent::Submit { position: at }));
        input(cx, PlatformInput::FileDrop(FileDropEvent::Ended));
        pause(cx, 200).await;
        if view(ws, cx).is_some() {
            break;
        }
        missed.push(name);
    }
    let dropped = view(ws, cx).is_some();
    let modal = cx.update(|_, cx| ws.read(cx).demo_modal(cx)).unwrap_or_default();
    let rep = import(ws, false, false, cx).await;
    let n = sql1(&core, &pg, &pw, "SELECT count(*) FROM e2e_import.e2e_parquet", cx).await;
    r.check("a Parquet file dropped on the window imports into the selected schema", exported && dropped && rep.is_some_and(|r| r.0 == 20) && n.as_deref() == Some("20"), format!("exported {exported} sheet {dropped} missed {missed:?} {rep:?} {n:?} modal {modal:?}"));

    let t = target(ws, &pg.id, &pg.database, "e2e_import", Some("e2e_csv"), cx);
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.import_data(t, Some(dir.join("e2e_csv.csv")), window, cx)));
    let rep = import(ws, true, false, cx).await;
    let n = sql1(&core, &pg, &pw, "SELECT count(*) FROM e2e_import.e2e_csv", cx).await;
    r.check("into an existing table with Truncate (past the guard): replaced, not doubled", rep.is_some() && n.as_deref() == Some("20"), format!("{rep:?} {n:?}"));

    let t = target(ws, &pg.id, &pg.database, "e2e_import", None, cx);
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.import_data(t, Some(dir.join("e2e_latin.csv")), window, cx)));
    let rep = import(ws, false, false, cx).await;
    let name = sql1(&core, &pg, &pw, "SELECT name FROM e2e_import.e2e_latin", cx).await;
    r.check("a Latin-1 file is read as Latin-1", rep.is_some() && name.as_deref() == Some("São Paulo"), format!("{rep:?} {name:?}"));

    let _ = sql(&core, &pg, &pw, "CREATE TABLE e2e_import.e2e_bad (id int, amount numeric)", cx).await;
    let t = target(ws, &pg.id, &pg.database, "e2e_import", Some("e2e_bad"), cx);
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.import_data(t, Some(dir.join("e2e_bad.csv")), window, cx)));
    let rep = import(ws, false, true, cx).await;
    let n = sql1(&core, &pg, &pw, "SELECT count(*) FROM e2e_import.e2e_bad", cx).await;
    r.check("on error Skip: the bad row is skipped, the others written", rep.is_some_and(|r| r.1 == 1) && n.as_deref() == Some("2"), format!("{rep:?} {n:?}"));
    let _ = sql(&core, &pg, &pw, "DROP SCHEMA e2e_import CASCADE", cx).await;

    let _ = std::fs::remove_dir_all(&dir);
}
