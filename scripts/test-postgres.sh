#!/usr/bin/env bash
#
# test-postgres.sh - start the throwaway Postgres clusters the server's integration tests need.
#
# CS-14's tests run against a real database and fail loudly when one is absent, because a suite
# that goes green on a machine which never ran it is worse than no suite. This makes satisfying
# that cheap, so the rule does not quietly become something people route around.
#
# The clusters live under the system temp directory and touch nothing else: not the developer's
# own Postgres, not their data directory, not port 5432. Stop them with `--stop`; delete them by
# deleting the directories.
#
# Usage:
#   eval "$(./scripts/test-postgres.sh)"     # start both, and export the two URLs
#   ./scripts/test-postgres.sh --stop
#
# # Why two clusters
#
# `changes_after` withholds rows whose inserting transaction may still be in flight (D-063), and it
# decides that from `pg_snapshot_xmin`, which reflects the oldest transaction running **anywhere in
# the cluster** — not in the database, in the *cluster*. Measured at CS-17: a transaction held open
# in a completely unrelated database still caused a committed row to read back as 0 of 1, and it
# appeared the instant that transaction ended. Transaction ids are cluster-wide, so a separate
# database isolates nothing.
#
# The migration tests (#63) deliberately hold write transactions open for tens of seconds. On one
# shared cluster that would withhold rows from every concurrently running pull test and make the
# suite flake for reasons that have nothing to do with the code under test. So they get their own
# cluster, on their own port, where the only thing their long transactions can delay is themselves.
#
# The underlying liveness problem is tracked as #62; this script works around it for the tests, it
# does not fix it.
#
# CI does not use this: GitHub Actions supplies Postgres as service containers. See
# .github/workflows/rust.yml.
set -euo pipefail

DBNAME=credsync_test
DBUSER=credsync

# The socket directories are kept short on purpose: a Unix socket path over 103 bytes is refused,
# and the default temp directory on macOS is long enough to blow that on its own.
MAIN_PORT="${CREDSYNC_TEST_PG_PORT:-55432}"
MAIN_PGDATA="${CREDSYNC_TEST_PGDATA:-${TMPDIR:-/tmp}/credsync-test-pg}"
MAIN_SOCK="${CREDSYNC_TEST_PG_SOCK:-/tmp/credsync-pg}"

ISO_PORT="${CREDSYNC_TEST_PG_ISOLATED_PORT:-55433}"
ISO_PGDATA="${CREDSYNC_TEST_PGDATA_ISOLATED:-${TMPDIR:-/tmp}/credsync-test-pg-isolated}"
ISO_SOCK="${CREDSYNC_TEST_PG_SOCK_ISOLATED:-/tmp/credsync-pg-iso}"

command -v initdb >/dev/null 2>&1 || {
  echo "error: initdb not found. Install PostgreSQL (brew install postgresql@16)." >&2
  exit 1
}

if [ "${1:-}" = "--stop" ]; then
  pg_ctl -D "$MAIN_PGDATA" stop >/dev/null 2>&1 || true
  pg_ctl -D "$ISO_PGDATA" stop >/dev/null 2>&1 || true
  echo "stopped (data left at $MAIN_PGDATA and $ISO_PGDATA)" >&2
  exit 0
fi

# Starts one cluster and creates its database. Idempotent.
start_cluster() {
  local pgdata="$1" port="$2" sockdir="$3" label="$4"

  mkdir -p "$sockdir"

  if [ ! -f "$pgdata/PG_VERSION" ]; then
    # `--auth=trust` is correct here and nowhere else: this cluster listens on loopback, holds only
    # test fixtures, and exists so a password does not stand between a developer and running the
    # tests.
    initdb -D "$pgdata" -U "$DBUSER" --auth=trust >/dev/null 2>&1
    echo "initialised $pgdata" >&2
  fi

  if ! pg_isready -h 127.0.0.1 -p "$port" >/dev/null 2>&1; then
    pg_ctl -D "$pgdata" \
           -o "-p $port -k $sockdir -h 127.0.0.1" \
           -l "$pgdata/server.log" start >/dev/null 2>&1
    for _ in $(seq 1 20); do
      pg_isready -h 127.0.0.1 -p "$port" >/dev/null 2>&1 && break
      sleep 0.3
    done
  fi

  pg_isready -h 127.0.0.1 -p "$port" >/dev/null 2>&1 || {
    echo "error: the $label Postgres did not come up. See $pgdata/server.log" >&2
    exit 1
  }

  psql -h 127.0.0.1 -p "$port" -U "$DBUSER" -d postgres \
       -tAc "SELECT 1 FROM pg_database WHERE datname = '$DBNAME'" | grep -q 1 || \
    psql -h 127.0.0.1 -p "$port" -U "$DBUSER" -d postgres \
         -c "CREATE DATABASE $DBNAME" >/dev/null

  echo "postgres ($label) ready on 127.0.0.1:$port" >&2
}

start_cluster "$MAIN_PGDATA" "$MAIN_PORT" "$MAIN_SOCK" "main"
start_cluster "$ISO_PGDATA" "$ISO_PORT" "$ISO_SOCK" "isolated"

# Printed on stdout so the whole thing can be `eval`ed; everything else goes to stderr.
echo "export CREDSYNC_TEST_DATABASE_URL=postgres://$DBUSER@127.0.0.1:$MAIN_PORT/$DBNAME"
echo "export CREDSYNC_TEST_ISOLATED_DATABASE_URL=postgres://$DBUSER@127.0.0.1:$ISO_PORT/$DBNAME"
