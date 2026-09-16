#!/usr/bin/env bash
#
# restore-drill.sh — the drill. Snapshot real data, back it up, DESTROY it,
# restore it from the off-box copy, and prove the result is identical.
#
# This is the script that turns "restorable" from an adjective into evidence.
# It is destructive on purpose: it drops the drill database and deletes the file
# store, so the only remaining copy is the encrypted backup. Then it restores
# from the REMOTE copy (not the local one) and diffs the data.
#
# Sequence:
#   1. snapshot the database (table set, row counts, per-table content hash) and
#      the file store (per-file sha256)
#   2. run deploy/backup.sh  -> encrypted set, uploaded to the "remote"
#   3. assert the artifacts really are OpenPGP-encrypted
#   4. DESTROY: drop the database, delete the file store
#   5. run deploy/restore.sh --from <remote>  (off-box copy is the only source)
#   6. snapshot again and diff; exit non-zero on any difference
#
# Usage:
#   deploy/restore-drill.sh --dsn postgres://user:pw@host:5432/jiuyue_drill \
#                           --backup-dir /tmp/drill/backups \
#                           --files-dir  /tmp/drill/files \
#                           --passphrase-file /tmp/drill/pass
#
# Options: --dsn, --backup-dir, --files-dir, --passphrase-file, --remote,
#          --work-dir, --stamp, --keep (do not destroy afterwards), --help.
#
# The same script is what .github/workflows/backup-restore.yml runs. It has no
# Linux-only dependencies, so it also runs against a local PostgreSQL on a dev
# machine (see docs/drills/).

set -Eeuo pipefail
IFS=$'\n\t'

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

DSN=""
BACKUP_DIR=""
FILES_DIR=""
PASSPHRASE_FILE=""
REMOTE=""
WORK_DIR=""
STAMP=""
KEEP=0

PSQL="${PSQL:-psql}"
PG_RESTORE="${PG_RESTORE:-pg_restore}"

log() { printf '%s [drill] %s\n' "$(date -u +%FT%TZ)" "$*" >&2; }
die() { printf '%s [drill] ERROR: %s\n' "$(date -u +%FT%TZ)" "$*" >&2; exit 1; }

usage() {
  sed -n '3,32p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit "${1:-0}"
}

cleanup() {
  local code=$?
  trap - EXIT
  if [ -n "$WORK_DIR" ] && [ -d "$WORK_DIR" ] && [ "$KEEP" -eq 0 ]; then
    rm -rf -- "$WORK_DIR"
  elif [ -n "$WORK_DIR" ]; then
    log "work directory kept: ${WORK_DIR}"
  fi
  exit "$code"
}
trap cleanup EXIT

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --dsn)             DSN="${2:-}"; shift 2 ;;
      --backup-dir)      BACKUP_DIR="${2:-}"; shift 2 ;;
      --files-dir)       FILES_DIR="${2:-}"; shift 2 ;;
      --passphrase-file) PASSPHRASE_FILE="${2:-}"; shift 2 ;;
      --remote)          REMOTE="${2:-}"; shift 2 ;;
      --work-dir)        WORK_DIR="${2:-}"; shift 2 ;;
      --stamp)           STAMP="${2:-}"; shift 2 ;;
      --keep)            KEEP=1; shift ;;
      -h|--help)         usage 0 ;;
      *)                 die "unknown argument: $1 (see --help)" ;;
    esac
  done
  [ -n "$DSN" ]             || die "--dsn is required"
  [ -n "$BACKUP_DIR" ]      || die "--backup-dir is required"
  [ -n "$FILES_DIR" ]       || die "--files-dir is required"
  [ -n "$PASSPHRASE_FILE" ] || die "--passphrase-file is required"
  [ -f "$PASSPHRASE_FILE" ] || die "passphrase file not found: ${PASSPHRASE_FILE}"
}

need_cmd() { command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"; }

dbname_from_dsn() {
  printf '%s' "$1" | sed -E 's#^.*/([^/?]+)(\?.*)?$#\1#'
}

maintenance_dsn_from() {
  printf '%s' "$1" | sed -E 's#^(.*://[^/]*)/[^/?]*(.*)$#\1/postgres\2#'
}

# The set of application tables, discovered from the live database so the drill
# compares whatever schema is actually deployed rather than a hard-coded list.
list_tables() {
  # libpq convention: options first, the database name last. Put the DSN last
  # everywhere, so the same call works on Linux and under MSYS on Windows.
  "$PSQL" -X -tA -c \
    "SELECT tablename FROM pg_tables WHERE schemaname='public' AND tablename <> '_sqlx_migrations' ORDER BY tablename" \
    "$1"
}

snapshot_db() {
  local dsn="$1" out="$2" t count
  : > "$out"
  while IFS= read -r t; do
    [ -n "$t" ] || continue
    count="$("$PSQL" -X -tA -c "SELECT count(*) FROM \"${t}\"" "$dsn" | tr -d '[:space:]')"
    printf 'table %s rows %s ' "$t" "$count" >> "$out"
    # Exact serialized bytes of every row, in a deterministic order.
    "$PSQL" -X -tA -c "COPY (SELECT * FROM \"${t}\" ORDER BY 1,2) TO STDOUT" "$dsn" \
      | sha256sum | awk '{print "sha256 " $1}' >> "$out"
  done < <(list_tables "$dsn" | tr -d '\r')
}

snapshot_files() {
  local dir="$1" out="$2"
  : > "$out"
  if [ -d "$dir" ]; then
    ( cd "$dir" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum ) > "$out" 2>/dev/null || true
  fi
}

assert_encrypted() {
  local f="$1" first
  [ -f "$f" ] || die "expected artifact is missing: $f"
  # An OpenPGP message starts with a packet header whose high bit is set; gzip
  # (1f), tar and plaintext all fail this. Cheap, and it catches the one mistake
  # that matters: shipping the file in the clear.
  first="$(od -An -tx1 -N1 "$f" | tr -d '[:space:]')"
  case "$first" in
    8?|9?|a?|b?|c?|d?|e?|f?) ;;
    *) die "artifact is not an OpenPGP message (first byte 0x${first}); encryption-before-upload failed: $f" ;;
  esac
  # Then prove it parses as a real OpenPGP message. The passphrase is supplied
  # on purpose: a bare `--list-packets` blocks on a pinentry prompt when the
  # first packet is an encrypted session key.
  if ! gpg --batch --quiet --pinentry-mode loopback --passphrase-file "$PASSPHRASE_FILE" \
        --list-packets "$f" >/dev/null 2>&1; then
    die "artifact does not parse as an OpenPGP message: $f"
  fi
}

run_backup() {
  local stamp="$1"
  (
    export DATABASE_URL="$DSN"
    export BACKUP_DIR="$BACKUP_DIR" DATA_DIR="$FILES_DIR" GPG_PASSPHRASE_FILE="$PASSPHRASE_FILE"
    export BACKUP_REMOTE="$REMOTE"
    export ENV_FILE="${WORK_DIR}-no-such-env"   # DATABASE_URL comes from the environment
    export BACKUP_KEEP_LOCAL_DAYS=7
    export BACKUP_KEEP_REMOTE_DAYS=30
    export HEARTBEAT_URL="${HEARTBEAT_URL:-}"
    export BACKUP_LOCK=""
    bash "${SCRIPT_DIR}/backup.sh" --date "$stamp"
  )
}

run_restore() {
  local stamp="$1"
  (
    export BACKUP_DIR="$BACKUP_DIR" DATA_DIR="$FILES_DIR" GPG_PASSPHRASE_FILE="$PASSPHRASE_FILE"
    export BACKUP_REMOTE="$REMOTE"
    export ENV_FILE="${WORK_DIR}-no-such-env"
    bash "${SCRIPT_DIR}/restore.sh" --stamp "$stamp" --from "$REMOTE" --dsn "$DSN" \
      --files-dir "$FILES_DIR" --clean
  )
}

destroy() {
  local dsn="$1" files_dir="$2" dbname admin
  dbname="$(dbname_from_dsn "$dsn")"
  admin="$(maintenance_dsn_from "$dsn")"
  log "DESTROYING the database '${dbname}' and the file store '${files_dir}'"
  "$PSQL" -X -v ON_ERROR_STOP=1 -q \
    -c "DROP DATABASE IF EXISTS \"${dbname}\" WITH (FORCE)" \
    "$admin" \
    || die "could not drop database ${dbname}"
  rm -rf -- "$files_dir"
  # Also purge the LOCAL copies, so the restore below can only succeed from the
  # off-box copy. This is the difference between "we have backups" and "we can
  # come back from losing the box".
  find "$BACKUP_DIR" -maxdepth 1 -type f -name 'jiuyue-*' -delete
  if "$PSQL" -X -tA -c "SELECT 1 FROM pg_database WHERE datname='${dbname}'" "$admin" | grep -q 1; then
    die "database ${dbname} still exists after the destroy step"
  fi
  [ ! -d "$files_dir" ] || die "file store ${files_dir} still exists after the destroy step"
  if find "$BACKUP_DIR" -maxdepth 1 -type f -name 'jiuyue-*' | grep -q .; then
    die "local backup artifacts survive the destroy step; the drill would not prove the remote copy"
  fi
  log "destroyed database, file store and local backup copies — only ${REMOTE} remains"
}

main() {
  parse_args "$@"
  need_cmd "$PSQL"
  need_cmd "$PG_RESTORE"
  need_cmd gpg
  need_cmd sha256sum
  need_cmd od

  [ -n "$REMOTE" ] || REMOTE="${BACKUP_DIR}/remote"
  mkdir -p -- "$BACKUP_DIR" "$REMOTE"
  if [ -z "$WORK_DIR" ]; then
    WORK_DIR="$(mktemp -d)"
  else
    mkdir -p -- "$WORK_DIR"
  fi
  STAMP="${STAMP:-$(date -u +%Y%m%dT%H%M%SZ)}"

  local started finished t_backup t_restore
  started="$(date -u +%s)"

  log "=== step 1/6: snapshot the live data (source of truth) ==="
  snapshot_db    "$DSN"      "${WORK_DIR}/before.db.txt"
  snapshot_files "$FILES_DIR" "${WORK_DIR}/before.files.txt"
  local files_before
  files_before="$(wc -l < "${WORK_DIR}/before.files.txt" | tr -d ' ')"
  [ "$files_before" -gt 0 ] \
    || die "the file store ${FILES_DIR} is empty — the drill would not prove file coverage"
  log "snapshot taken: $(wc -l < "${WORK_DIR}/before.db.txt" | tr -d ' ') db lines, ${files_before} files"

  log "=== step 2/6: back up (encrypt, upload to ${REMOTE}) ==="
  t_backup="$(date -u +%s)"
  run_backup "$STAMP"
  log "backup took $(( $(date -u +%s) - t_backup ))s"

  log "=== step 3/6: assert the artifacts are encrypted ==="
  assert_encrypted "${REMOTE}/jiuyue-${STAMP}.db.dump.gz.gpg"
  assert_encrypted "${REMOTE}/jiuyue-${STAMP}.files.tar.gz.gpg"
  log "both remote artifacts are OpenPGP-encrypted (verify at rest, not just in transit)"

  log "=== step 4/6: DESTROY the data ==="
  destroy "$DSN" "$FILES_DIR"

  log "=== step 5/6: restore from the REMOTE copy ==="
  t_restore="$(date -u +%s)"
  run_restore "$STAMP"
  log "restore took $(( $(date -u +%s) - t_restore ))s"

  log "=== step 6/6: compare ==="
  snapshot_db    "$DSN"      "${WORK_DIR}/after.db.txt"
  snapshot_files "$FILES_DIR" "${WORK_DIR}/after.files.txt"

  local ok=1
  if diff -u "${WORK_DIR}/before.db.txt" "${WORK_DIR}/after.db.txt" > "${WORK_DIR}/db.diff"; then
    log "database: IDENTICAL ($(wc -l < "${WORK_DIR}/before.db.txt" | tr -d ' ') lines compared)"
  else
    log "database: DIFFERS — see below"
    cat "${WORK_DIR}/db.diff" >&2
    ok=0
  fi
  if diff -u "${WORK_DIR}/before.files.txt" "${WORK_DIR}/after.files.txt" > "${WORK_DIR}/files.diff"; then
    log "file store: IDENTICAL (${files_before} files compared)"
  else
    log "file store: DIFFERS — see below"
    cat "${WORK_DIR}/files.diff" >&2
    ok=0
  fi

  finished="$(date -u +%s)"
  if [ "$ok" -eq 1 ]; then
    log "DRILL PASSED — destroy + restore round-tripped with no discrepancy"
    log "backup $((t_backup - started))s, restore $((t_restore - t_backup))s, total $((finished - started))s"
  else
    log "DRILL FAILED — the backup does not reproduce the original data"
  fi
  [ "$ok" -eq 1 ]
}

main "$@"
