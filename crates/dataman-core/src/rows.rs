//! Small query helpers for the core's own statements: text rows with their column names, and
//! typed rows decoded like the grid's ([`Kind`](crate::value::Kind)s).

use crate::backend::{Db, RowSource};
use crate::result::{Cell, Chunk, ChunkBuilder, ColumnInfo, ErrorInfo, format_cell};

/// Rows of a simple-protocol query with the column names (known even when no row comes back).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextRows {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

impl TextRows {
    pub fn col(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c == name)
    }

    /// Value of column `name` in row `row`.
    pub fn get(&self, row: usize, name: &str) -> Option<&str> {
        self.rows.get(row)?.get(self.col(name)?)?.as_deref()
    }
}

/// Rows decoded into columnar chunks, like a result in the grid.
pub struct TypedRows {
    pub columns: Vec<ColumnInfo>,
    chunks: Vec<Chunk>,
    starts: Vec<usize>,
    rows: usize,
}

impl TypedRows {
    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    pub fn cell(&self, row: usize, col: usize) -> Cell<'_> {
        let i = match self.starts.binary_search(&row) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        self.chunks[i].cell(row - self.starts[i], col)
    }

    pub fn text(&self, row: usize, col: usize) -> Option<String> {
        format_cell(self.columns[col].kind, self.cell(row, col)).map(|s| s.into_owned())
    }

    pub fn col(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }
}

/// Runs `sql` through the typed protocol, keeping at most `max` rows (the rest is read and
/// dropped).
pub(crate) async fn typed_query(db: &Db, sql: &str, max: usize) -> Result<TypedRows, ErrorInfo> {
    let p = db.prepare(sql).await?;
    let mut rows = p.start().await?;
    let mut b = ChunkBuilder::new(&p.columns);
    let mut out = TypedRows { columns: p.columns.clone(), chunks: vec![], starts: vec![], rows: 0 };
    loop {
        let more = if out.rows + b.len() >= max { rows.skip().await? } else { rows.next_into(&mut b).await? };
        if !more {
            break;
        }
        if b.len() >= crate::result::CHUNK {
            out.starts.push(out.rows);
            out.rows += b.len();
            out.chunks.push(b.finish());
        }
    }
    if !b.is_empty() {
        out.starts.push(out.rows);
        out.rows += b.len();
        out.chunks.push(b.finish());
    }
    Ok(out)
}
