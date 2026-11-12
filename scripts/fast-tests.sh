#!/bin/bash
# Prints the `cargo test` selection CI runs: every test, minus the files below. Each test in them
# needs a database server and skips itself without one, so where none is up they cost minutes of
# linking and test nothing. A new test file is selected on its own; list it here only if all of
# its tests return early without a server.
#
#   cargo test --workspace $(scripts/fast-tests.sh)
set -euo pipefail
cd "$(dirname "$0")/.."
needs_a_server=" live live_activity live_agent live_agent_guard live_batch live_dataview live_ddl live_estimate live_explain live_fk live_import live_intel live_mssql live_mssql_catalog live_mssql_ddl live_mssql_edit live_mssql_results live_mssql_types live_my_ms_complete live_mysql live_mysql_catalog live_mysql_ddl live_mysql_edit live_mysql_results live_mysql_types live_next live_objects live_redis live_ssh live_tls live_tx live_versions live_weird_names "
printf -- '--lib --bins'
for f in crates/*/tests/*.rs; do basename "$f" .rs; done | sort -u | while read -r n; do
  case "$needs_a_server" in *" $n "*) ;; *) printf ' --test %s' "$n" ;; esac
done
echo
