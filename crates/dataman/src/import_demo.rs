//! Demo stages of the data import (`--demo`), against the demo database. The files are written
//! to a temporary folder when the stage starts; nothing is loaded into the database (the
//! progress and failure views get fixed numbers).
//!
//! - `import-data`: a `;`-separated CSV into `public.orders`, mapped by name (screenshot);
//! - `import-data-menu`: the same with a column's mapping menu open (screenshot);
//! - `import-data-new`: an NDJSON file into a new table, with inferred types (screenshot);
//! - `import-data-guard`: truncate first, Import: the guard sheet (screenshot);
//! - `import-data-run`: the progress view (screenshot);
//! - `import-data-failed`: a failed load with its line (screenshot).

use std::path::PathBuf;
use std::time::Duration;

use gpui::{AsyncWindowContext, Entity};

use crate::app::Workspace;
use crate::data_import::DataTarget;

pub fn is_stage(s: &str) -> bool {
    s.starts_with("import-data")
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

pub fn demo_dir() -> PathBuf {
    std::env::temp_dir().join(format!("dataman-demo-import-{}", std::process::id()))
}

/// `orders-2024.csv`: the columns of `orders` (one in another case), and one it lacks.
fn orders_csv() -> String {
    let status = ["delivered", "shipped", "paid", "cancelled", "new"];
    let mut s = String::from("id;Customer;status;created_at;total;address;notes\n");
    for i in 0..120u64 {
        let id = 90_001 + i;
        let customer = 10_000 + (i * 7_919) % 80_000;
        let st = status[(i % 5) as usize];
        let created_at = 1_735_700_000 + i * 86_400;
        let total = 1_990 + (i * 3_571) % 250_000;
        let notes = match i % 4 {
            0 => "",
            1 => "deliver after 6 pm",
            2 => "\"fragile; handle with care\"",
            _ => "leave at the front desk",
        };
        s.push_str(&format!(
            "{id};{customer};{st};{created_at};{total};\"{{\"\"zip\"\": \"\"{:08}\"\", \"\"city\"\": \"\"City {}\"\"}}\";{notes}\n",
            20_000_000 + i * 13,
            i % 500
        ));
    }
    s
}

/// `events.ndjson`: integers, text, timestamps, decimals, objects, booleans, uuids.
fn events_ndjson() -> String {
    let kinds = ["login", "checkout", "search", "logout"];
    (0..80u64)
        .map(|i| {
            format!(
                "{{\"id\": {}, \"kind\": \"{}\", \"at\": \"2024-03-{:02}T{:02}:{:02}:00Z\", \"amount\": {}.{:02}, \"payload\": {{\"ip\": \"10.0.{}.{}\", \"tags\": [\"a\", \"b\"]}}, \"ok\": {}, \"user_id\": \"0f8fad5b-d9cb-469f-a165-7086772895{:02x}\"}}\n",
                i + 1,
                kinds[(i % 4) as usize],
                1 + i % 28,
                8 + i % 12,
                (i * 7) % 60,
                (i * 37) % 900,
                i % 100,
                i % 10,
                i % 250,
                i % 3 != 0,
                i % 256
            )
        })
        .collect()
}

pub async fn run(ws: &Entity<Workspace>, stage: &str, id: &str, db: &str, cx: &mut AsyncWindowContext) {
    let dir = demo_dir();
    let _ = std::fs::create_dir_all(&dir);
    let new = stage == "import-data-new";
    let path = if new { dir.join("events.ndjson") } else { dir.join("orders-2024.csv") };
    let _ = std::fs::write(&path, if new { events_ndjson() } else { orders_csv() });
    let Ok(Some(conn)) = cx.update(|_, cx| ws.read(cx).store.read(cx).connection(id)) else { return };
    let target = DataTarget {
        conn_id: id.to_string(),
        conn,
        db: db.to_string(),
        schema: "public".into(),
        table: (!new).then(|| "orders".to_string()),
    };
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.import_data(Some(target), Some(path.clone()), window, cx)));
    let mut view = None;
    for _ in 0..200 {
        pause(cx, 50).await;
        let v = cx.update(|_, cx| ws.read(cx).data_import.clone()).ok().flatten();
        if let Some(v) = v
            && cx.update(|_, cx| v.read(cx).demo_ready()).unwrap_or(false)
        {
            view = Some(v);
            break;
        }
    }
    let Some(view) = view else {
        eprintln!("import demo: the sheet did not get ready");
        return;
    };
    let _ = cx.update(|_, cx| {
        view.update(cx, |v, cx| match stage {
            "import-data-menu" => v.demo_menu(1, cx),
            "import-data-guard" => {
                v.demo_set_truncate(true);
                v.start(cx);
            }
            "import-data-run" => v.demo_progress(cx),
            "import-data-failed" => v.demo_failed(cx),
            _ => {}
        })
    });
    pause(cx, 300).await;
}
