//! Demo stages of the explorer's objects (`--demo`), against the demo database: the schema
//! `diskman_shop` and the roles `diskman_reader` and `diskman_app`, created from
//! `seeds/explorer.sql` at the start.
//!
//! - `tree-objects`: the schema's groups, with its tables (row estimates and sizes), functions
//!   and types open, and the enum's labels (screenshot);
//! - `tree-table`: `orders` with its constraints and triggers, `events` with its partitions
//!   (screenshot);
//! - `tree-filter`: the filter `order` over every loaded kind (screenshot);
//! - `privileges`: the grant matrix of `orders` with `diskman_app` picked (screenshot).

use std::time::Duration;

use gpui::{AsyncWindowContext, Entity};

use crate::app::Workspace;
use crate::tree::SchemaTree;
use crate::tree_menu::{Act, Target};

pub const SEED: &str = include_str!("../../../seeds/explorer.sql");

pub fn is_stage(s: &str) -> bool {
    matches!(s, "tree-objects" | "tree-table" | "tree-filter" | "privileges")
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

/// Waits (up to 10 s) for a row to be in the tree.
async fn row(tree: &Entity<SchemaTree>, id: &str, cx: &mut AsyncWindowContext) -> bool {
    for _ in 0..200 {
        if cx.update(|_, cx| tree.read(cx).has_row(id)).unwrap_or(false) {
            return true;
        }
        pause(cx, 50).await;
    }
    eprintln!("explorer demo: no row {id}");
    false
}

/// Expands a node once it is there, and gives its children time to load.
async fn expand(tree: &Entity<SchemaTree>, id: &str, cx: &mut AsyncWindowContext) {
    if row(tree, id, cx).await {
        let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.expand_id(id, cx)));
        pause(cx, 350).await;
    }
}

async fn collapse(tree: &Entity<SchemaTree>, id: &str, cx: &mut AsyncWindowContext) {
    if row(tree, id, cx).await {
        let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.collapse_id(id, cx)));
    }
}

pub async fn run(ws: &Entity<Workspace>, stage: &str, id: &str, db: &str, cx: &mut AsyncWindowContext) {
    crate::table_check::run_seed(ws, SEED, cx).await;
    let Ok(tree) = cx.update(|_, cx| ws.read(cx).tree.clone()) else { return };
    // The seed changed the catalog: the connection's schema is read again.
    let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.refresh_connection(id, cx)));
    let c = format!("c:{id}");
    let s = format!("{c}/d:{db}/s:diskman_shop");
    let r = |n: &str| format!("{s}/r:{n}");
    if matches!(stage, "tree-objects" | "tree-table") {
        // A wider explorer, as dragged: signatures and definitions get their room.
        let _ = cx.update(|_, cx| {
            ws.update(cx, |w, cx| {
                w.explorer_w = 380.;
                cx.notify();
            })
        });
    }
    expand(&tree, &s, cx).await;
    match stage {
        "tree-objects" => {
            for g in ["tables", "functions", "types"] {
                expand(&tree, &format!("{s}/{g}"), cx).await;
            }
            expand(&tree, &format!("{s}/types/t:order_status"), cx).await;
        }
        "tree-table" => {
            expand(&tree, &format!("{s}/tables"), cx).await;
            for (table, subs) in [("orders", ["cons", "trg"]), ("events", ["parts", ""])] {
                expand(&tree, &r(table), cx).await;
                collapse(&tree, &format!("{}/cols", r(table)), cx).await;
                for sub in subs.iter().filter(|s| !s.is_empty()) {
                    expand(&tree, &format!("{}/{sub}", r(table)), cx).await;
                }
            }
        }
        "tree-filter" => {
            // Every kind loaded, then the filter.
            for g in ["tables", "functions", "sequences", "types", "triggers"] {
                expand(&tree, &format!("{s}/{g}"), cx).await;
            }
            let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.set_filter("order", cx)));
        }
        "privileges" => {
            expand(&tree, &format!("{s}/tables"), cx).await;
            row(&tree, &r("orders"), cx).await;
            let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.select_id(&r("orders"), cx)));
            let target = Target::Rel {
                conn: id.to_string(),
                db: db.to_string(),
                schema: "diskman_shop".into(),
                name: "orders".into(),
                kind: diskman_core::schema::RelKind::Table,
                lar: false,
                ro: false,
            };
            let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.on_row_action(Act::Privileges, target, false, window, cx)));
            for _ in 0..100 {
                pause(cx, 50).await;
                let ready = cx
                    .update(|_, cx| match &ws.read(cx).side {
                        Some(crate::app::Side::Feature("privileges", v, _)) => {
                            v.clone().downcast::<crate::privileges::PrivilegesPanel>().ok().map(|p| (p.read(cx).is_ready(), p))
                        }
                        _ => None,
                    })
                    .ok()
                    .flatten();
                if let Some((true, p)) = ready {
                    let _ = cx.update(|_, cx| p.update(cx, |p, cx| p.pick("diskman_app", cx)));
                    break;
                }
            }
        }
        _ => {}
    }
    pause(cx, 600).await;
}
