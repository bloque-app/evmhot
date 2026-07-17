#!/usr/bin/env bash
set -euo pipefail

# One-command dry-run helper for the SQLite -> Postgres data migration.
#
# Pulls the newest production SQLite snapshot out of the S3 backup prefix
# written by substrate-rail/entrypoint.sh (DATA_BACKUP_ENABLED=true writes
# timestamped folders under DATA_BACKUP_S3_URI), folds its WAL sidecar into
# the main file, runs it through `migrate_sqlite_to_postgres` against a
# scratch Postgres database, then verifies per-table counts + next_index +
# last_block:<chain> cursors before declaring success. Never touches the S3
# snapshot in place and never runs against a database that already has data
# unless --force is passed through.
#
# Usage:
#   POSTGRES_URL=postgres://postgres:test@localhost:5433/evmhot \
#     ./scripts/migrate_data_to_postgres.sh --s3-uri s3://bloque-substrate-rail-backups/prod
#
#   # Or point at a local snapshot directly, skipping S3:
#   POSTGRES_URL=postgres://postgres:test@localhost:5433/evmhot \
#     ./scripts/migrate_data_to_postgres.sh --local-wallet-db /path/to/wallet.db
#
# Required:
#   POSTGRES_URL          destination postgres://... connection string (never hardcoded)
# One of:
#   --s3-uri URI          DATA_BACKUP_S3_URI prefix to pull the latest snapshot from
#   --local-wallet-db PATH  skip S3, use an already-local wallet.db (+ -wal/-shm if present)
# Optional:
#   --tls disable|verify-ca   passed through to the migration binary (default: verify-ca)
#   --force                   allow migrating into a non-empty destination (idempotent inserts)
#   --keep-tmp                 don't delete the scratch dir on exit (for inspection)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

S3_URI=""
LOCAL_WALLET_DB=""
TLS_MODE="verify-ca"
FORCE=0
KEEP_TMP=0

usage() {
  cat <<EOF
Usage:
  POSTGRES_URL=postgres://user:pass@host:5432/db $(basename "$0") --s3-uri s3://bucket/prefix
  POSTGRES_URL=postgres://user:pass@host:5432/db $(basename "$0") --local-wallet-db /path/to/wallet.db

Options:
  --s3-uri URI            DATA_BACKUP_S3_URI prefix; picks the newest timestamped snapshot folder
  --local-wallet-db PATH  use an existing local wallet.db instead of pulling from S3
  --tls MODE              'verify-ca' (default) or 'disable' (local/Docker Postgres)
  --force                 allow migrating into a non-empty destination
  --keep-tmp              keep the scratch directory instead of deleting it on exit
  -h, --help              show this help
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --s3-uri)
      S3_URI="$2"
      shift 2
      ;;
    --local-wallet-db)
      LOCAL_WALLET_DB="$2"
      shift 2
      ;;
    --tls)
      TLS_MODE="$2"
      shift 2
      ;;
    --force)
      FORCE=1
      shift
      ;;
    --keep-tmp)
      KEEP_TMP=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "Error: unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [ -z "${POSTGRES_URL:-}" ]; then
  echo "Error: POSTGRES_URL env var is required (destination postgres://... connection string)" >&2
  exit 1
fi

if [ -z "${S3_URI}" ] && [ -z "${LOCAL_WALLET_DB}" ]; then
  echo "Error: pass either --s3-uri or --local-wallet-db" >&2
  usage >&2
  exit 1
fi

if [ -n "${S3_URI}" ] && [ -n "${LOCAL_WALLET_DB}" ]; then
  echo "Error: pass only one of --s3-uri or --local-wallet-db" >&2
  exit 1
fi

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/evmhot-pg-migrate.XXXXXX")"
cleanup() {
  if [ "${KEEP_TMP}" -eq 1 ]; then
    echo "migrate_data_to_postgres: keeping scratch dir: ${TMP_DIR}"
  else
    rm -rf "${TMP_DIR}"
  fi
}
trap cleanup EXIT

fetch_latest_s3_snapshot() {
  local uri="$1"
  local prefix="${uri%/}/"

  echo "migrate_data_to_postgres: listing snapshots under ${prefix}"
  local latest
  latest="$(aws s3 ls "${prefix}" | awk '{print $2}' | grep -E '^[0-9-]+/$' | sort | tail -n1)"
  if [ -z "${latest}" ]; then
    echo "Error: no timestamped snapshot folders found under ${prefix}" >&2
    exit 1
  fi

  local snapshot_uri="${prefix}${latest}"
  echo "migrate_data_to_postgres: newest snapshot: ${snapshot_uri}"

  local local_dir="${TMP_DIR}/snapshot"
  mkdir -p "${local_dir}"
  for f in wallet.db wallet.db-wal wallet.db-shm; do
    if aws s3 ls "${snapshot_uri}${f}" >/dev/null 2>&1; then
      aws s3 cp "${snapshot_uri}${f}" "${local_dir}/${f}"
    fi
  done

  if [ ! -f "${local_dir}/wallet.db" ]; then
    echo "Error: ${snapshot_uri}wallet.db not found" >&2
    exit 1
  fi

  echo "${local_dir}/wallet.db"
}

stage_local_snapshot() {
  local src="$1"
  local local_dir="${TMP_DIR}/snapshot"
  mkdir -p "${local_dir}"

  cp "${src}" "${local_dir}/wallet.db"
  for sidecar in -wal -shm; do
    if [ -f "${src}${sidecar}" ]; then
      cp "${src}${sidecar}" "${local_dir}/wallet.db${sidecar}"
    fi
  done

  echo "${local_dir}/wallet.db"
}

fold_wal() {
  # Opening the copy once with sqlite3 and forcing a full checkpoint folds any
  # -wal sidecar into the main file, so the migration binary (which opens the
  # file read-only) sees a fully consistent, self-contained snapshot. Falls
  # back to a no-op with a warning if sqlite3 isn't on PATH -- the migration
  # binary can still read a WAL-mode file directly, just less deterministically.
  local db_path="$1"

  if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "migrate_data_to_postgres: warning: sqlite3 not found on PATH, skipping WAL fold" >&2
    return 0
  fi

  echo "migrate_data_to_postgres: folding WAL into ${db_path} (PRAGMA wal_checkpoint(TRUNCATE))"
  sqlite3 "${db_path}" "PRAGMA wal_checkpoint(TRUNCATE);" >/dev/null
}

echo "migrate_data_to_postgres: scratch dir: ${TMP_DIR}"

if [ -n "${S3_URI}" ]; then
  WALLET_DB="$(fetch_latest_s3_snapshot "${S3_URI}")"
else
  WALLET_DB="$(stage_local_snapshot "${LOCAL_WALLET_DB}")"
fi

fold_wal "${WALLET_DB}"

MIGRATE_ARGS=(--from "${WALLET_DB}" --to "${POSTGRES_URL}" --tls "${TLS_MODE}")
if [ "${FORCE}" -eq 1 ]; then
  MIGRATE_ARGS+=(--force)
fi

echo "migrate_data_to_postgres: running migration binary"
( cd "${REPO_DIR}" && cargo run --quiet --bin migrate_sqlite_to_postgres -- "${MIGRATE_ARGS[@]}" )

echo
echo "migrate_data_to_postgres: verifying (pass 1)"
( cd "${REPO_DIR}" && cargo run --quiet --bin migrate_sqlite_to_postgres -- \
    --from "${WALLET_DB}" --to "${POSTGRES_URL}" --tls "${TLS_MODE}" --verify )

echo
echo "migrate_data_to_postgres: re-running migration to confirm idempotent no-op"
( cd "${REPO_DIR}" && cargo run --quiet --bin migrate_sqlite_to_postgres -- \
    --from "${WALLET_DB}" --to "${POSTGRES_URL}" --tls "${TLS_MODE}" --force )

echo
echo "migrate_data_to_postgres: verifying (pass 2, post re-run)"
( cd "${REPO_DIR}" && cargo run --quiet --bin migrate_sqlite_to_postgres -- \
    --from "${WALLET_DB}" --to "${POSTGRES_URL}" --tls "${TLS_MODE}" --verify )

echo
echo "migrate_data_to_postgres: PASS -- migration verified idempotent against ${WALLET_DB}"
