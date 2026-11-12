#!/bin/bash
# Runs the engines' older releases and the servers that speak their protocols, one at a time:
# each service of e2e/versions/*.yaml is started, the tests its labels name run against it, and
# it is removed before the next one. Local only (no CI): a few of the images are amd64-only and
# run through Rosetta on Apple silicon.
#
# Each service's labels say how it is tested:
#   dataman.url     its connection string (DATAMAN_VERSIONS_URL for tests/live_versions.rs)
#   dataman.expect  what its version starts with (DATAMAN_VERSIONS_EXPECT)
#   dataman.env     more variables, for the older suites that read their own (DATAMAN_PG=…)
#   dataman.tests   the dataman-core test files run against it
#
#   scripts/versions.sh                 every service
#   scripts/versions.sh pg-12 mysql-    the services whose names start with these
#   scripts/versions.sh --list          the services and what each runs
#   KEEP=1 scripts/versions.sh pg-12    leaves the server up afterwards
set -euo pipefail
cd "$(dirname "$0")/.."
log=target/versions
mkdir -p "$log"

ctx=$(docker context show)
[ "$(uname)" = Darwin ] && [ "$ctx" != orbstack ] && { echo "docker context is $ctx, not orbstack: docker context use orbstack" >&2; exit 2; }

list=0
filters=()
for a in "$@"; do
  case "$a" in --list) list=1 ;; *) filters+=("$a") ;; esac
done

# One line per service: file, name, url, expect, env, tests (tab separated).
services() {
  for f in e2e/versions/*.yaml; do
    docker compose -f "$f" config --format json | jq -r --arg f "$f" '.services | to_entries[] |
      [$f, .key, .value.labels["dataman.url"], .value.labels["dataman.expect"] // "", .value.labels["dataman.env"] // "", .value.labels["dataman.tests"]] | @tsv'
  done
}

picked() {
  [ ${#filters[@]} -eq 0 ] && return 0
  for p in "${filters[@]}"; do [[ "$1" == "$p"* ]] && return 0; done
  return 1
}

rows=()
while IFS=$'\t' read -r file svc url expect env tests; do
  picked "$svc" && rows+=("$file"$'\t'"$svc"$'\t'"$url"$'\t'"$expect"$'\t'"$env"$'\t'"$tests")
done < <(services)
[ ${#rows[@]} -eq 0 ] && { echo "no service matches: ${filters[*]}" >&2; exit 2; }

if [ $list = 1 ]; then
  for r in "${rows[@]}"; do IFS=$'\t' read -r file svc url expect env tests <<<"$r"; printf '%-16s %-28s %s\n' "$svc" "$expect" "$tests"; done
  exit 0
fi

# Every test binary any picked service needs, built before a server starts.
all_tests=$(for r in "${rows[@]}"; do IFS=$'\t' read -r _ _ _ _ _ tests <<<"$r"; echo $tests; done | tr ' ' '\n' | sort -u)
targs=()
for t in $all_tests; do targs+=(--test "$t"); done
cargo test -p dataman-core "${targs[@]}" --no-run -q

summary=()
failed=0
for r in "${rows[@]}"; do
  IFS=$'\t' read -r file svc url expect env tests <<<"$r"
  start=$SECONDS
  out="$log/$svc.log"
  result=ok
  echo "== $svc ($expect)"
  if ! docker compose -f "$file" up -d --wait --wait-timeout 600 "$svc" >"$out" 2>&1; then
    result="did not start"
  else
    targs=()
    for t in $tests; do targs+=(--test "$t"); done
    # shellcheck disable=SC2086
    if ! env DATAMAN_VERSIONS_URL="$url" DATAMAN_VERSIONS_EXPECT="$expect" $env \
        cargo test -p dataman-core "${targs[@]}" -q --no-fail-fast >>"$out" 2>&1; then
      result=failed
    fi
  fi
  [ "${KEEP:-0}" = 1 ] || docker compose -f "$file" rm -sfv "$svc" >>"$out" 2>&1
  [ "$result" = ok ] || failed=1
  summary+=("$(printf '%-16s %-14s %4ss  %s' "$svc" "$result" $((SECONDS - start)) "$out")")
  echo "   $result"
done

echo
printf '%s\n' "${summary[@]}"
exit $failed
