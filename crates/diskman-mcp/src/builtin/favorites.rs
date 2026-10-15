//! list_favorites, add_favorite, remove_favorite, import_favorites: the favorite queries saved
//! in `favorites.toml`, shared with the app (`crate::history` panel, ⌘Y).

use diskman_core::Favorite;
use serde_json::{Value, json};

use super::{Builtin, table};
use crate::tool::{Args, BoxFut, Call, ToolError};

pub fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "list_favorites",
            description: "Lists Diskman's favorite queries: statements saved across history pruning and \"clear history\" (only remove_favorite drops one). Rows: [id, name, connection, database, sql], connection and database null when the favorite is not pinned to one.",
            schema: crate::tool::Schema::new(),
            need: None,
            run: list_favorites,
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
