//! DuckDB's catalog: `duckdb_tables()`, `duckdb_views()`, `duckdb_columns()`… always filtered by
//! catalog (database) and schema.

use crate::sql::Dialect;

pub(crate) fn lit(s: &str) -> String {
    Dialect::DuckDb.quote_literal(s)
}

/// The primary key's columns, in order (the current catalog).
pub(crate) fn primary_key_sql(schema: &str, table: &str) -> String {
    format!(
        "SELECT unnest(constraint_column_names) FROM duckdb_constraints() \
          WHERE database_name = current_database() AND schema_name = {} AND table_name = {} AND constraint_type = 'PRIMARY KEY'",
        lit(schema),
        lit(table)
    )
}
