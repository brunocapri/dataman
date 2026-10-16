//! list_favorites, add_favorite, remove_favorite, import_favorites: the favorite queries saved
//! in `favorites.toml`, shared with the app (`crate::history` panel, ⌘Y).

use dataman_core::{Favorite, NameMatch};
use serde_json::{Map, Value, json};

use super::{Builtin, limits_props, table};
use crate::tool::{Args, BoxFut, Call, ToolError, params_schema};

pub fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "list_favorites",
            description: "Lists Dataman's favorite queries: statements saved across history pruning and \"clear history\" (only remove_favorite drops one). Rows: [id, name, connection, database, sql], connection and database null when the favorite is not pinned to one. Run one with run_favorite by its name (an approximate name is enough).",
            schema: crate::tool::Schema::new(),
            need: None,
            run: list_favorites,
        },
        Builtin {
            name: "run_favorite",
            description: "Runs one of the user's favorite queries by its name, as open_query runs SQL: visibly in the user's agent tab while Dataman is open, reads as run_query (results: […] for several statements), writes through the connection's MCP tier and its approvals; run: false only places it in the tab. The name may be approximate (case, accents, word order, typos): one clear match runs, and the answer names it (favorite). When several are close, or none is, nothing runs: {matched: false, reason, candidates: [{name, connection, database, sql}]} to choose from (ask the user if unsure, then call again with the exact name). Runs on the favorite's own connection and database unless connection is given.",
            schema: limits_props(
                crate::tool::Schema::new()
                    .prop("name", true, json!({"type": "string", "maxLength": 200, "description": "The favorite's name, or close to it (list_favorites)."}))
                    .prop("connection", false, json!({"type": "string", "description": "Connection name or id (list_connections); default: the favorite's own."}))
                    .prop("database", false, json!({"type": "string", "description": "Database on that connection; default: the favorite's own (with its connection), else the connection's."}))
                    .prop("params", false, params_schema())
                    .prop("run", false, json!({"type": "boolean", "description": "false: put the favorite's SQL in the user's agent tab without running it. Default true."})),
                200,
            ),
            need: None,
            run: run_favorite,
        },
        Builtin {
            name: "add_favorite",
            description: "Saves a favorite query. Deduplicates by statement text: adding one that matches an existing favorite (trimmed) returns that favorite's id instead of creating a duplicate.",
            schema: crate::tool::Schema::new()
                .prop("name", true, json!({"type": "string", "maxLength": 200, "description": "A short label."}))
                .prop("sql", true, json!({"type": "string", "maxLength": 100000, "description": "The statement, verbatim."}))
                .prop("connection", false, json!({"type": "string", "description": "Connection name or id (list_connections), to open it on."}))
                .prop("database", false, json!({"type": "string", "description": "Database on that connection."})),
            need: None,
            run: add_favorite,
        },
        Builtin {
            name: "remove_favorite",
            description: "Removes a favorite query by id (list_favorites). The only way a favorite goes away: history pruning and \"clear history\" never touch favorites. A missing id is not an error.",
            schema: crate::tool::Schema::new().prop("id", true, json!({"type": "string", "description": "The favorite's id, from list_favorites."})),
            need: None,
            run: remove_favorite,
        },
        Builtin {
            name: "rename_favorite",
            description: "Renames a favorite query by id (list_favorites); its statement, connection and database stay as they are.",
            schema: crate::tool::Schema::new()
                .prop("id", true, json!({"type": "string", "description": "The favorite's id, from list_favorites."}))
                .prop("name", true, json!({"type": "string", "maxLength": 200, "description": "The new name."})),
            need: None,
            run: rename_favorite,
        },
        Builtin {
            name: "import_favorites",
            description: "Adds each given .sql file as a favorite query: name is the file's name without extension, sql is the file's content verbatim. Deduplicates like add_favorite. Every path must exist and end in .sql.",
            schema: crate::tool::Schema::new().prop(
                "paths",
                true,
                json!({"type": "array", "minItems": 1, "maxItems": 50, "items": {"type": "string"}, "description": "Absolute paths to .sql files."}),
            ),
            need: None,
            run: import_favorites,
        },
    ]
}

fn failed(e: anyhow::Error) -> ToolError {
    ToolError::Failed(format!("{e:#}"))
}

fn list_favorites(cx: &Call, _a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        cx.enabled()?;
        let profiles = cx.hub.profiles();
        let rows = cx
            .hub
            .core
            .favorites
            .list()
            .into_iter()
            .map(|f| {
                let conn_name = f.connection_id.as_ref().and_then(|id| profiles.iter().find(|p| p.id == *id)).map(|p| p.name.clone());
                vec![json!(f.id), json!(f.name), conn_name.map(|n| json!(n)).unwrap_or(Value::Null), f.database.map(|d| json!(d)).unwrap_or(Value::Null), json!(f.sql)]
            })
            .collect();
        Ok(table(&["id", "name", "connection", "database", "sql"], rows))
    })
}

/// A statement's first line that is not a comment (a candidate's preview).
fn first_line(sql: &str) -> String {
    let line = sql.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("--")).unwrap_or("");
    match line.char_indices().nth(120) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_string(),
    }
}

fn run_favorite(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        cx.enabled()?;
        let query = a.str("name")?;
        let favorites = cx.hub.core.favorites.list();
        let profiles = cx.hub.profiles();
        let conn_name = |f: &Favorite| f.connection_id.as_ref().map(|id| profiles.iter().find(|p| p.id == *id).map(|p| p.name.clone()).unwrap_or_else(|| id.clone()));
        let names: Vec<&str> = favorites.iter().map(|f| f.name.as_str()).collect();
        // Its id names it too (from list_favorites).
        let found = match favorites.iter().position(|f| f.id == query) {
            Some(i) => NameMatch::One(i),
            None => dataman_core::favorites::match_name(&names, query),
        };
        let listed = |ix: &[usize]| -> Vec<Value> {
            ix.iter()
                .map(|&i| {
                    let f = &favorites[i];
                    json!({"name": f.name, "connection": conn_name(f), "database": f.database, "sql": first_line(&f.sql)})
                })
                .collect()
        };
        let f = match found {
            NameMatch::One(i) => &favorites[i],
            NameMatch::Ambiguous(ix) => {
                return Ok(json!({"matched": false, "reason": format!("{} favorites match {query:?} about as well; nothing ran", ix.len()), "candidates": listed(&ix)}));
            }
            NameMatch::Nothing(ix) => {
                let reason = if favorites.is_empty() { "there are no favorite queries".to_string() } else { format!("no favorite is named like {query:?}; nothing ran") };
                return Ok(json!({"matched": false, "reason": reason, "candidates": listed(&ix)}));
            }
        };
        cx.note_favorite(&f.name);
        let mut args: Map<String, Value> = a.0.clone();
        args.remove("name");
        args.insert("sql".into(), json!(f.sql));
        args.insert("title".into(), json!(f.name));
        if a.opt_str("connection").is_none() {
            let Some(c) = conn_name(f) else {
                return Err(ToolError::Invalid(format!("the favorite {:?} is not saved with a connection: pass connection (list_connections)", f.name)));
            };
            args.insert("connection".into(), json!(c));
            if a.opt_str("database").is_none()
                && let Some(d) = &f.database
            {
                args.insert("database".into(), json!(d));
            }
        }
        let mut v = super::query::open(cx, Args(args)).await?;
        if let Some(m) = v.as_object_mut() {
            let mut head = Map::new();
            head.insert("favorite".into(), json!({"id": f.id, "name": f.name}));
            head.extend(std::mem::take(m));
            *m = head;
        }
        Ok(v)
    })
}

fn add_favorite(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        cx.enabled()?;
        let name = a.str("name")?;
        let sql = a.str("sql")?;
        if sql.trim().is_empty() {
            return Err(ToolError::Invalid("\"sql\" must not be empty".into()));
        }
        let connection_id = match a.opt_str("connection") {
            Some(name) => Some(cx.hub.profile(name).map_err(ToolError::Invalid)?.id),
            None => None,
        };
        let mut fav = Favorite::new(name, sql);
        fav.connection_id = connection_id;
        fav.database = a.opt_str("database").map(str::to_string);
        let saved = cx.hub.core.favorites.add(fav).map_err(failed)?;
        Ok(json!({"id": saved.id, "name": saved.name}))
    })
}

fn remove_favorite(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        cx.enabled()?;
        let id = a.str("id")?;
        cx.hub.core.favorites.remove(id).map_err(failed)?;
        Ok(json!({"removed": id}))
    })
}

fn rename_favorite(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        cx.enabled()?;
        let id = a.str("id")?;
        let name = a.str("name")?;
        if name.trim().is_empty() {
            return Err(ToolError::Invalid("\"name\" must not be empty".into()));
        }
        match cx.hub.core.favorites.rename(id, name).map_err(failed)? {
            Some(f) => Ok(json!({"id": f.id, "name": f.name})),
            None => Err(ToolError::Invalid(format!("no favorite with id {id:?}"))),
        }
    })
}

fn import_favorites(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        cx.enabled()?;
        let paths = a.get("paths").and_then(Value::as_array).filter(|a| !a.is_empty()).ok_or_else(|| ToolError::Invalid("\"paths\" is required".into()))?;
        let mut added = Vec::new();
        for p in paths {
            let s = p.as_str().ok_or_else(|| ToolError::Invalid("\"paths\" items must be strings".into()))?;
            let path = std::path::Path::new(s);
            if !path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("sql")) {
                return Err(ToolError::Invalid(format!("{s:?} is not a .sql file")));
            }
            let sql = std::fs::read_to_string(path).map_err(|e| ToolError::Invalid(format!("{s:?}: {e}")))?;
            let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("favorite").to_string();
            let saved = cx.hub.core.favorites.add(Favorite::new(name, sql)).map_err(failed)?;
            added.push(json!({"id": saved.id, "name": saved.name}));
        }
        Ok(json!({"added": added}))
    })
}
