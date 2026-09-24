#!/usr/bin/env bash
#
# test-postgres.sh - start a throwaway Postgres for the server's integration tests.
#
# CS-14's tests run against a real database and fail loudly when one is absent, because a suite
# that goes green on a machine which never ran it is worse than no suite. This makes satisfying
# that cheap, so the rule does not quietly become something people route around.
#
# The cluster lives under the system temp directory and touches nothing else: not the developer's
# own Postgres, not their data directory, not port 5432. Stop it with `--stop`; delete it by
# deleting the directory.
#
# Usage:
#   eval "$(./scripts/test-postgres.sh)"     # start, and export CREDSYNC_TEST_DATABASE_URL
#   ./scripts/test-postgres.sh --stop
#
# CI does not use this: GitHub Actions supplies Postgres as a service container. See
# .github/workflows/rust.yml.
set -euo pipefail

PORT="${CREDSYNC_TEST_PG_PORT:-55432}"
PGDATA="${CREDSYNC_TEST_PGDATA:-${TMPDIR:-/tmp}/credsync-test-pg}"
# The socket directory is kept short on purpose: a Unix socket path over 103 bytes is refused, and
# the default temp directory on macOS is long enough to blow that on its own.
SOCKDIR="${CREDSYNC_TEST_PG_SOCK:-/tmp/credsync-pg}"
DBNAME=credsync_test
DBUSER=credsync

command -v initdb >/dev/null 2>&1 || {
  echo "error: initdb not found. Install PostgreSQL (brew install postgresql@16)." >&2
  exit 1
}

if [ "${1:-}" = "--stop" ]; then
  pg_ctl -D "$PGDATA" stop >/dev/null 2>&1 || true
  echo "stopped (data left at $PGDATA)" >&2
  exit 0
fi

mkdir -p "$SOCKDIR"

if [ ! -f "$PGDATA/PG_VERSION" ]; then
  # `--auth=trust` is correct here and nowhere else: this cluster listens on loopback, holds only
  # test fixtures, and exists so a password does not stand between a developer and running the
  # tests.
  initdb -D "$PGDATA" -U "$DBUSER" --auth=trust >/dev/null 2>&1
  echo "initialised $PGDATA" >&2
fi

if ! pg_isready -h 127.0.0.1 -p "$PORT" >/dev/null 2>&1; then
  pg_ctl -D "$PGDATA" \
         -o "-p $PORT -k $SOCKDIR -h 127.0.0.1" \
         -l "$PGDATA/server.log" start >/dev/null 2>&1
  for _ in $(seq 1 20); do
    pg_isready -h 127.0.0.1 -p "$PORT" >/dev/null 2>&1 && break
    sleep 0.3
  done
fi

pg_isready -h 127.0.0.1 -p "$PORT" >/dev/null 2>&1 || {
  echo "error: Postgres did not come up. See $PGDATA/server.log" >&2
  exit 1
}

psql -h 127.0.0.1 -p "$PORT" -U "$DBUSER" -d postgres \
     -tAc "SELECT 1 FROM pg_database WHERE datname = '$DBNAME'" | grep -q 1 || \
  psql -h 127.0.0.1 -p "$PORT" -U "$DBUSER" -d postgres \
       -c "CREATE DATABASE $DBNAME" >/dev/null

echo "postgres ready on 127.0.0.1:$PORT" >&2
# Printed on stdout so the whole thing can be `eval`ed; everything else goes to stderr.
echo "export CREDSYNC_TEST_DATABASE_URL=postgres://$DBUSER@127.0.0.1:$PORT/$DBNAME"
