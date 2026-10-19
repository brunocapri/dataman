//! The planner's estimate of the rows a paused result's statement returns ("Fetch all ~30k"):
//! asked once per result, never on the tab's session (the connection's metadata client on
//! PostgreSQL and MySQL, a connection of its own with SHOWPLAN on SQL Server), only for plain
//! reads without parameters. Nothing runs: `EXPLAIN` without ANALYZE, `SET SHOWPLAN_XML ON`.

use std::sync::Arc;

use crate::config::Engine;
use crate::guard::Effect;
use crate::result::ResultSet;
use crate::sql::Dialect;
use crate::{Connection, Task};

type Slot = tokio::sync::Mutex<Option<Arc<crate::backend::ms::Client>>>;

/// Whether `sql` is a plain read the estimate may be asked for: one `SELECT` / `WITH … SELECT` /
/// `TABLE` / `VALUES`, no `INTO`, nothing the guard sees as an effect.
pub fn estimable(sql: &str, dialect: Dialect) -> bool {
    let stmts = crate::sql::split_in(sql, dialect);
    let mut real = stmts.iter().filter(|s| !sql[s.range.clone()].trim().is_empty());
    let (Some(s), None) = (real.next(), real.next()) else { return false };
    let text = &sql[s.range.clone()];
    let first = text.split_whitespace().next().unwrap_or("").trim_start_matches('(').to_ascii_uppercase();
    if !["SELECT", "WITH", "TABLE", "VALUES"].contains(&first.as_str()) {
        return false;
    }
    let f = crate::guard::classify_in(text, dialect);
    f.effect == Effect::Read && crate::guard::read_only_refusal_in(text, dialect).is_none()
}

impl Connection {
    /// Asks for `rs`'s estimate once (the first call; later ones do nothing) and stores the
    /// answer on it ([`ResultSet::row_estimate`]); `None` where there is none (LAR, MySQL before
    /// 5.7, a statement with parameters or not a plain read, no permission, an error).
    pub fn ask_row_estimate(self: &Arc<Self>, database: &str, rs: &Arc<ResultSet>) {
        if !rs.ask_estimate() {
            return;
        }
        let dialect = self.dialect();
        if !rs.params().is_empty() || self.info.is_lar || !estimable(&rs.sql, dialect) || (self.engine() == Engine::MySql && !mysql_explains_safely(&self.info.version)) {
            rs.set_estimate(None);
            return;
        }
        let (this, db, rs) = (self.clone(), database.to_string(), rs.clone());
        // (Dropping the task handle does not stop the work.)
        let _: Task<()> = self.spawn(async move {
            let found = this.estimate(&db, rs.sql.trim().trim_end_matches(';')).await;
            rs.set_estimate(found);
            Ok(())
        });
    }

    async fn estimate(self: &Arc<Self>, db: &str, sql: &str) -> Option<u64> {
        match self.engine() {
            Engine::Postgres => {
                let c = self.meta(db).await.ok()?;
                let rows = crate::backend::text_rows(&c, &format!("EXPLAIN (FORMAT JSON) {sql}")).await.ok()?;
                pg_plan_rows(rows.first()?.first()?.as_deref()?)
            }
            Engine::MySql => {
                let c = self.meta(db).await.ok()?;
                let rows = crate::backend::text_rows(&c, &format!("EXPLAIN FORMAT=JSON {sql}")).await.ok()?;
                mysql_plan_rows(rows.first()?.first()?.as_deref()?)
            }
            Engine::SqlServer => {
                let slot = self.showplan_slot(db);
                let mut g = slot.lock().await;
                if g.as_ref().is_none_or(|c| c.is_closed()) {
                    let crate::backend::Dedicated::Ms(c) = self.dedicated(Some(db)).await.ok()? else { return None };
                    *g = Some(c);
                }
                let c = g.clone()?;
                c.text_rows("SET SHOWPLAN_XML ON").await.ok()?;
                let plan = c.text_rows(sql).await;
                // Back off, whatever the plan did (the connection is this check's alone, but
                // a later use must find it plain).
                if c.text_rows("SET SHOWPLAN_XML OFF").await.is_err() {
                    *g = None;
                }
                showplan_rows(plan.ok()?.first()?.first()?.as_deref()?)
            }
            _ => None,
        }
    }

    fn showplan_slot(&self, db: &str) -> Arc<Slot> {
        let key = ("showplan", db.to_string(), String::new());
        let mut g = self.caches.lock();
        if let Some(s) = g.get(&key).cloned().and_then(|a| a.downcast::<Slot>().ok()) {
            return s;
        }
        let s: Arc<Slot> = Arc::new(tokio::sync::Mutex::new(None));
        g.insert(key, s.clone());
        s
    }
}

/// MySQL 5.7+ and MariaDB 10+: `EXPLAIN` there never runs a derived table (older MySQL did).
pub fn mysql_explains_safely(version: &str) -> bool {
    let v = version.trim_start_matches("MySQL ").trim_start_matches("MariaDB ");
    let mut parts = v.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    if version.contains("MariaDB") {
        return major >= 10;
    }
    major > 5 || (major == 5 && minor >= 7)
}

/// `EXPLAIN (FORMAT JSON)`: the top plan node's "Plan Rows".
pub fn pg_plan_rows(json: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let rows = v.get(0)?.get("Plan")?.get("Plan Rows")?.as_f64()?;
    (rows >= 0.).then_some(rows.round() as u64)
}

/// `EXPLAIN FORMAT=JSON` (MySQL, MariaDB): the rows the outermost operation produces: a single
/// table's `rows_produced_per_join` (else `rows_examined_per_scan`, MariaDB's `rows`), a
/// join's last table's; through ordering, grouping and duplicate removal wrappers.
pub fn mysql_plan_rows(json: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let mut node = v.get("query_block")?;
    for _ in 0..8 {
        if let Some(t) = node.get("table") {
            return table_rows(t);
        }
        if let Some(n) = node.get("nested_loop").and_then(|n| n.as_array()) {
            return table_rows(n.last()?.get("table")?);
        }
        node = ["ordering_operation", "grouping_operation", "duplicates_removal", "windowing", "read_sorted_file", "filesort", "temporary_table"]
            .iter()
            .find_map(|k| node.get(k))?;
    }
    None
}

fn table_rows(t: &serde_json::Value) -> Option<u64> {
    let num = |k: &str| t.get(k).and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok())));
    let rows = num("rows_produced_per_join").or_else(|| num("rows_examined_per_scan")).or_else(|| num("rows"))?;
    (rows >= 0.).then_some(rows.round() as u64)
}

/// `SET SHOWPLAN_XML ON`'s plan: the first statement's `StatementEstRows`.
pub fn showplan_rows(xml: &str) -> Option<u64> {
    let at = xml.find("StatementEstRows=\"")? + "StatementEstRows=\"".len();
    let rest = &xml[at..];
    let v: f64 = rest[..rest.find('"')?].parse().ok()?;
    (v >= 0.).then_some(v.round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans() {
        assert_eq!(pg_plan_rows(r#"[{"Plan": {"Node Type": "Seq Scan", "Plan Rows": 30000, "Plan Width": 8}}]"#), Some(30000));
        assert_eq!(pg_plan_rows("[]"), None);
        let my = r#"{"query_block": {"select_id": 1, "cost_info": {"query_cost": "1.0"}, "table": {"table_name": "customer", "rows_examined_per_scan": 5, "rows_produced_per_join": 5}}}"#;
        assert_eq!(mysql_plan_rows(my), Some(5));
        let join = r#"{"query_block": {"ordering_operation": {"nested_loop": [{"table": {"rows_produced_per_join": 5}}, {"table": {"rows_examined_per_scan": 2, "rows_produced_per_join": 12.5}}]}}}"#;
        assert_eq!(mysql_plan_rows(join), Some(13));
        assert_eq!(mysql_plan_rows(r#"{"query_block": {"table": {"rows": 42}}}"#), Some(42), "MariaDB");
        let xml = r#"<ShowPlanXML><BatchSequence><Batch><Statements><StmtSimple StatementText="SELECT" StatementId="1" StatementEstRows="1234.5" StatementType="SELECT">"#;
        assert_eq!(showplan_rows(xml), Some(1235));
        assert_eq!(showplan_rows("<x/>"), None);
    }

    #[test]
    fn what_is_asked() {
        let pg = Dialect::Postgres;
        assert!(estimable("SELECT * FROM orders", pg));
        assert!(estimable("with x as (select 1) select * from x;", pg));
        assert!(!estimable("SELECT * INTO t2 FROM orders", pg));
        assert!(!estimable("UPDATE orders SET a = 1", pg));
        assert!(!estimable("SELECT 1; SELECT 2", pg));
        assert!(!estimable("EXEC dbo.p", Dialect::TSql));
        assert!(!estimable("WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d", pg));
        assert!(mysql_explains_safely("MySQL 8.4.3") && mysql_explains_safely("MySQL 5.7.44") && mysql_explains_safely("MariaDB 10.11.6"));
        assert!(!mysql_explains_safely("MySQL 5.6.51"));
    }
}
