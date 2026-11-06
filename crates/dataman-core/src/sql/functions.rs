//! Every function the guard and the agents' rules treat apart, in one table, each with what it
//! does: what refuses it on a read, on a write, on a file database or where data is kept from
//! agents all reads this, so the lists cannot drift apart.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Does(u8);

/// Effects a read-only transaction does not stop (signals, files, locks held past the
/// transaction, other servers) or that only PostgreSQL stops there.
pub const EFFECT: Does = Does(1);
/// Runs SQL handed to it as a string: what the string does is never checked
/// (`query_to_xml('select pg_terminate_backend(1)', …)`).
pub const STRING_SQL: Does = Does(1 << 1);
/// Reaches the database server's own files, or runs a program there.
pub const SERVER_FILE: Does = Does(1 << 2);
/// SQLite: loads code or touches files (the shell's; an application may register them too).
pub const SQLITE_FILE: Does = Does(1 << 3);
/// DuckDB: reads (and runs) whatever it names: files, other databases, SQL in a string, secrets,
/// the instance every connection to the file shares and its logs.
pub const DUCKDB_FILE: Does = Does(1 << 4);
/// ClickHouse: table functions, table engines and dictionary sources that read (and write) files,
/// URLs, buckets and other servers, or read tables whole past what the agents' rules narrow.
pub const CLICKHOUSE_FILE: Does = Does(1 << 7);
/// Reads tables, schemas or files whole, around the agents' rules on columns.
pub const READS_AROUND: Does = Does(1 << 5);
/// An effect a write is allowed to have (a sequence, a notification).
pub const WRITE_OK: Does = Does(1 << 6);

impl Does {
    pub const NONE: Does = Does(0);

    pub fn has(self, what: Does) -> bool {
        self.0 & what.0 != 0
    }
}

impl std::ops::BitOr for Does {
    type Output = Does;
    fn bitor(self, o: Does) -> Does {
        Does(self.0 | o.0)
    }
}

const fn or(a: Does, b: Does) -> Does {
    Does(a.0 | b.0)
}

macro_rules! table {
    ($(($name:literal, $first:ident $(| $more:ident)*)),* $(,)?) => {
        /// Lowercase names.
        pub const FUNCTIONS: &[(&str, Does)] = &[$(($name, { let d = $first; $(let d = or(d, $more);)* d })),*];
    };
}

table! {
    ("arrowflight", CLICKHOUSE_FILE),
    ("azureblobstorage", CLICKHOUSE_FILE),
    ("azureblobstoragecluster", CLICKHOUSE_FILE),
    ("azurequeue", CLICKHOUSE_FILE),
    ("backup", CLICKHOUSE_FILE),
    ("buffer", CLICKHOUSE_FILE),
    ("cassandra", CLICKHOUSE_FILE),
    ("catboostevaluate", CLICKHOUSE_FILE),
    ("clickhouse", CLICKHOUSE_FILE),
    ("cluster", CLICKHOUSE_FILE),
    ("clusterallreplicas", CLICKHOUSE_FILE),
    ("columns", CLICKHOUSE_FILE),
    ("cos", CLICKHOUSE_FILE),
    ("cosn", CLICKHOUSE_FILE),
    ("datalakecatalog", CLICKHOUSE_FILE),
    ("deltalake", CLICKHOUSE_FILE),
    ("deltalakeazure", CLICKHOUSE_FILE),
    ("deltalakecluster", CLICKHOUSE_FILE),
    ("deltalakelocal", CLICKHOUSE_FILE),
    ("deltalakes3", CLICKHOUSE_FILE),
    ("dictionary", CLICKHOUSE_FILE),
    ("dictisin", CLICKHOUSE_FILE),
    ("distributed", CLICKHOUSE_FILE),
    ("executable", CLICKHOUSE_FILE),
    ("executable_pool", CLICKHOUSE_FILE),
    ("executablepool", CLICKHOUSE_FILE),
    ("file", CLICKHOUSE_FILE),
    ("filecluster", CLICKHOUSE_FILE),
    ("filelog", CLICKHOUSE_FILE),
    ("filesystem", CLICKHOUSE_FILE),
    ("gcs", CLICKHOUSE_FILE),
    ("generateserialid", CLICKHOUSE_FILE),
    ("hascolumnintable", CLICKHOUSE_FILE),
    ("hdfs", CLICKHOUSE_FILE),
    ("hdfscluster", CLICKHOUSE_FILE),
    ("hive", CLICKHOUSE_FILE),
    ("http", CLICKHOUSE_FILE),
    ("hudi", CLICKHOUSE_FILE),
    ("hudicluster", CLICKHOUSE_FILE),
    ("iceberg", CLICKHOUSE_FILE),
    ("icebergazure", CLICKHOUSE_FILE),
    ("icebergazurecluster", CLICKHOUSE_FILE),
    ("iceberghdfs", CLICKHOUSE_FILE),
    ("iceberghdfscluster", CLICKHOUSE_FILE),
    ("iceberglocal", CLICKHOUSE_FILE),
    ("icebergs3", CLICKHOUSE_FILE),
    ("icebergs3cluster", CLICKHOUSE_FILE),
    ("input", CLICKHOUSE_FILE),
    ("jdbc", CLICKHOUSE_FILE),
    ("joinget", CLICKHOUSE_FILE),
    ("joingetornull", CLICKHOUSE_FILE),
    ("kafka", CLICKHOUSE_FILE),
    ("keepermap", CLICKHOUSE_FILE),
    ("logtrace", CLICKHOUSE_FILE),
    ("loop", CLICKHOUSE_FILE),
    ("materializedmysql", CLICKHOUSE_FILE),
    ("materializedpostgresql", CLICKHOUSE_FILE),
    ("materializemysql", CLICKHOUSE_FILE),
    ("merge", CLICKHOUSE_FILE),
    ("mergetreeindex", CLICKHOUSE_FILE),
    ("mergetreeprojection", CLICKHOUSE_FILE),
    ("mongodb", CLICKHOUSE_FILE),
    ("mysql", CLICKHOUSE_FILE),
    ("nats", CLICKHOUSE_FILE),
    ("odbc", CLICKHOUSE_FILE),
    ("oss", CLICKHOUSE_FILE),
    ("paimon", CLICKHOUSE_FILE),
    ("postgresql", CLICKHOUSE_FILE),
    ("prometheusquery", CLICKHOUSE_FILE),
    ("prometheusqueryrange", CLICKHOUSE_FILE),
    ("rabbitmq", CLICKHOUSE_FILE),
    ("redis", CLICKHOUSE_FILE),
    ("remote", CLICKHOUSE_FILE),
    ("remotesecure", CLICKHOUSE_FILE),
    ("s3", CLICKHOUSE_FILE),
    ("s3cluster", CLICKHOUSE_FILE),
    ("s3queue", CLICKHOUSE_FILE),
    ("sqlite", CLICKHOUSE_FILE),
    ("timeseriesdata", CLICKHOUSE_FILE),
    ("timeseriesmetrics", CLICKHOUSE_FILE),
    ("timeseriesselector", CLICKHOUSE_FILE),
    ("timeseriestags", CLICKHOUSE_FILE),
    ("url", CLICKHOUSE_FILE),
    ("urlcluster", CLICKHOUSE_FILE),
    ("view", CLICKHOUSE_FILE),
    ("viewexplain", CLICKHOUSE_FILE),
    ("viewifpermitted", CLICKHOUSE_FILE),
    ("yamlregexptree", CLICKHOUSE_FILE),
    ("ytsaurus", CLICKHOUSE_FILE),
    ("pg_terminate_backend", EFFECT),
    ("pg_cancel_backend", EFFECT),
    ("pg_reload_conf", EFFECT),
    ("pg_rotate_logfile", EFFECT),
    ("pg_switch_wal", EFFECT),
    ("pg_create_restore_point", EFFECT),
    ("pg_promote", EFFECT),
    ("pg_wal_replay_pause", EFFECT),
    ("pg_wal_replay_resume", EFFECT),
    ("pg_log_backend_memory_contexts", EFFECT),
    ("pg_advisory_lock", EFFECT),
    ("pg_advisory_lock_shared", EFFECT),
    ("pg_try_advisory_lock", EFFECT),
    ("pg_try_advisory_lock_shared", EFFECT),
    ("pg_advisory_unlock", EFFECT),
    ("pg_advisory_unlock_all", EFFECT),
    ("pg_advisory_unlock_shared", EFFECT),
    ("lo_import", EFFECT | SERVER_FILE | READS_AROUND),
    ("lo_export", EFFECT | SERVER_FILE),
    ("lo_unlink", EFFECT),
    ("lo_create", EFFECT),
    ("lo_creat", EFFECT),
    ("lo_from_bytea", EFFECT),
    ("lo_put", EFFECT),
    ("lowrite", EFFECT),
    ("pg_file_write", EFFECT | SERVER_FILE),
    ("pg_file_rename", EFFECT | SERVER_FILE),
    ("pg_file_unlink", EFFECT | SERVER_FILE),
    ("pg_file_sync", EFFECT | SERVER_FILE),
    ("pg_read_file", EFFECT | SERVER_FILE | READS_AROUND),
    ("pg_read_binary_file", EFFECT | SERVER_FILE | READS_AROUND),
    ("pg_ls_dir", EFFECT | SERVER_FILE),
    ("pg_stat_file", EFFECT | SERVER_FILE),
    ("pg_ls_logdir", EFFECT | SERVER_FILE),
    ("pg_ls_waldir", EFFECT | SERVER_FILE),
    ("pg_ls_tmpdir", EFFECT | SERVER_FILE),
    ("pg_ls_archive_statusdir", EFFECT | SERVER_FILE),
    ("dblink", EFFECT | STRING_SQL | READS_AROUND),
    ("dblink_exec", EFFECT | STRING_SQL | READS_AROUND),
    ("dblink_connect", EFFECT),
    ("dblink_connect_u", EFFECT),
    ("dblink_send_query", EFFECT | STRING_SQL),
    ("dblink_open", EFFECT | STRING_SQL),
    ("pg_notify", EFFECT | WRITE_OK),
    ("nextval", EFFECT | WRITE_OK),
    ("setval", EFFECT),
    ("pg_stat_reset", EFFECT),
    ("pg_stat_reset_shared", EFFECT),
    ("pg_stat_reset_single_table_counters", EFFECT),
    ("pg_stat_statements_reset", EFFECT),
    ("pg_drop_replication_slot", EFFECT),
    ("pg_create_physical_replication_slot", EFFECT),
    ("pg_create_logical_replication_slot", EFFECT),
    ("pg_logical_emit_message", EFFECT),
    ("pg_replication_origin_create", EFFECT),
    ("pg_replication_origin_drop", EFFECT),
    ("pg_import_system_collations", EFFECT),
    ("txid_current", EFFECT | WRITE_OK),
    ("pg_current_xact_id", EFFECT | WRITE_OK),
    ("set_config", EFFECT),
    ("get_lock", EFFECT),
    ("release_lock", EFFECT),
    ("release_all_locks", EFFECT),
    ("load_file", EFFECT | SERVER_FILE | READS_AROUND),
    ("master_pos_wait", EFFECT),
    ("source_pos_wait", EFFECT),
    ("wait_for_executed_gtid_set", EFFECT),
    ("openrowset", EFFECT | STRING_SQL | SERVER_FILE | READS_AROUND),
    ("openquery", EFFECT | STRING_SQL | READS_AROUND),
    ("opendatasource", EFFECT | STRING_SQL | SERVER_FILE | READS_AROUND),
    ("xp_cmdshell", EFFECT | SERVER_FILE),
    ("xp_dirtree", EFFECT | SERVER_FILE),
    ("xp_subdirs", EFFECT | SERVER_FILE),
    ("xp_fileexist", EFFECT | SERVER_FILE),
    ("xp_regwrite", EFFECT | SERVER_FILE),
    ("sp_oacreate", EFFECT | SERVER_FILE),
    ("sp_oamethod", EFFECT | SERVER_FILE),
    ("sp_addextendedproc", EFFECT | SERVER_FILE),
    ("query_to_xml", STRING_SQL | READS_AROUND),
    ("query_to_xmlschema", STRING_SQL | READS_AROUND),
    ("query_to_xml_and_xmlschema", STRING_SQL | READS_AROUND),
    ("cursor_to_xml", STRING_SQL | READS_AROUND),
    ("cursor_to_xmlschema", STRING_SQL | READS_AROUND),
    ("ts_stat", STRING_SQL | READS_AROUND),
    ("ts_rewrite", STRING_SQL | READS_AROUND),
    ("xpath_table", STRING_SQL | READS_AROUND),
    ("crosstab", STRING_SQL),
    ("crosstab2", STRING_SQL),
    ("crosstab3", STRING_SQL),
    ("crosstab4", STRING_SQL),
    ("connectby", STRING_SQL),
    ("sp_executesql", STRING_SQL | READS_AROUND),
    ("load_extension", SQLITE_FILE | DUCKDB_FILE),
    ("readfile", SQLITE_FILE),
    ("writefile", SQLITE_FILE),
    ("edit", SQLITE_FILE),
    ("fts3_tokenizer", SQLITE_FILE),
    ("fsdir", SQLITE_FILE),
    ("read_csv", DUCKDB_FILE),
    ("read_csv_auto", DUCKDB_FILE),
    ("csv_scan", DUCKDB_FILE),
    ("sniff_csv", DUCKDB_FILE),
    ("read_parquet", DUCKDB_FILE),
    ("parquet_scan", DUCKDB_FILE),
    ("parquet_metadata", DUCKDB_FILE),
    ("parquet_schema", DUCKDB_FILE),
    ("parquet_file_metadata", DUCKDB_FILE),
    ("parquet_kv_metadata", DUCKDB_FILE),
    ("parquet_bloom_probe", DUCKDB_FILE),
    ("read_json", DUCKDB_FILE),
    ("read_json_auto", DUCKDB_FILE),
    ("read_json_objects", DUCKDB_FILE),
    ("read_json_objects_auto", DUCKDB_FILE),
    ("read_ndjson", DUCKDB_FILE),
    ("read_ndjson_auto", DUCKDB_FILE),
    ("read_ndjson_objects", DUCKDB_FILE),
    ("json_scan", DUCKDB_FILE),
    ("read_text", DUCKDB_FILE),
    ("read_blob", DUCKDB_FILE),
    ("glob", DUCKDB_FILE),
    ("read_xlsx", DUCKDB_FILE),
    ("st_read", DUCKDB_FILE),
    ("iceberg_scan", DUCKDB_FILE),
    ("iceberg_metadata", DUCKDB_FILE),
    ("iceberg_snapshots", DUCKDB_FILE),
    ("delta_scan", DUCKDB_FILE),
    ("sqlite_scan", DUCKDB_FILE),
    ("sqlite_attach", DUCKDB_FILE),
    ("postgres_scan", DUCKDB_FILE),
    ("postgres_attach", DUCKDB_FILE),
    ("postgres_query", DUCKDB_FILE),
    ("mysql_scan", DUCKDB_FILE),
    ("mysql_query", DUCKDB_FILE),
    ("query", DUCKDB_FILE),
    ("query_table", DUCKDB_FILE),
    ("duckdb_secrets", DUCKDB_FILE),
    ("which_secret", DUCKDB_FILE),
    ("getenv", DUCKDB_FILE),
    ("read_duckdb", DUCKDB_FILE),
    ("parquet_full_metadata", DUCKDB_FILE),
    ("json_execute_serialized_sql", DUCKDB_FILE),
    ("arrow_scan", DUCKDB_FILE),
    ("arrow_scan_dumb", DUCKDB_FILE),
    ("enable_logging", DUCKDB_FILE),
    ("disable_logging", DUCKDB_FILE),
    ("truncate_duckdb_logs", DUCKDB_FILE),
    ("enable_profiling", DUCKDB_FILE),
    ("disable_profiling", DUCKDB_FILE),
    ("checkpoint", DUCKDB_FILE),
    ("force_checkpoint", DUCKDB_FILE),
    ("duckdb_logs", DUCKDB_FILE),
    ("duckdb_logs_parsed", DUCKDB_FILE),
    ("duckdb_temporary_files", DUCKDB_FILE),
    ("duckdb_external_file_cache", DUCKDB_FILE),
    ("duckdb_prepared_statements", DUCKDB_FILE),
    ("lo_get", READS_AROUND),
    ("table_to_xml", READS_AROUND),
    ("table_to_xmlschema", READS_AROUND),
    ("table_to_xml_and_xmlschema", READS_AROUND),
    ("schema_to_xml", READS_AROUND),
    ("schema_to_xmlschema", READS_AROUND),
    ("schema_to_xml_and_xmlschema", READS_AROUND),
    ("database_to_xml", READS_AROUND),
    ("database_to_xmlschema", READS_AROUND),
    ("database_to_xml_and_xmlschema", READS_AROUND),
}

pub fn does(name: &str) -> Does {
    let n = name.trim_matches(['"', '`', '[', ']']);
    FUNCTIONS.iter().find(|(f, _)| f.eq_ignore_ascii_case(n)).map_or(Does::NONE, |&(_, d)| d)
}

pub fn is(name: &str, what: Does) -> bool {
    does(name).has(what)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_entry_per_function() {
        for (i, (n, d)) in FUNCTIONS.iter().enumerate() {
            assert_eq!(*n, n.to_lowercase(), "{n}");
            assert!(!FUNCTIONS[..i].iter().any(|(m, _)| m == n), "{n} twice");
            assert_ne!(*d, Does::NONE, "{n}");
            // What reaches the server's files is refused on reads too.
            assert!(!d.has(SERVER_FILE) || d.has(EFFECT), "{n} is refused on writes but not on reads");
        }
        assert!(is("QUERY_TO_XML", STRING_SQL) && is("query_to_xml", READS_AROUND) && is("\"pg_read_file\"", SERVER_FILE));
        assert!(is("nextval", WRITE_OK) && is("nextval", EFFECT) && !is("upper", EFFECT));
        assert!(is("S3", CLICKHOUSE_FILE) && !is("url", EFFECT), "a name other engines may use is refused on ClickHouse only");
        assert!(!is("buffer", READS_AROUND | STRING_SQL) && !is("hive", READS_AROUND | STRING_SQL), "`agent_sql` refuses those words anywhere, on every engine");
    }
}
