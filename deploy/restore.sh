#!/usr/bin/env bash
#
# restore.sh — rebuild a working database AND file store from one backup set.
#
# This is the other half of deploy/backup.sh, and the half that decides whether a
# backup was real. It takes a backup set, verifies it, and puts both halves back:
#
#   * the database  — decrypt -> gunzip -> `pg_restore` into the target database
#   * the file store — decrypt -> untar into the target directory (ADR-0006)
#
# It REFUSES to restore an archive whose checksum sidecar does not verify, using
# the same sha256-or-refuse discipline as deploy/deploy.sh. A corrupt or tampered
# archive is not a backup.
#
# No containers anywhere (ADR-0010): native `pg_restore`, native `tar`, native `gpg`.
#
# Usage:
#   # newest set found in BACKUP_REMOTE / BACKUP_DIR:
#   sudo deploy/restore.sh --stamp 20260917T003000Z
#
#   # explicit artifacts:
#   sudo deploy/restore.sh --db artifact.db.dump.gz.gpg --files artifact.files.tar.gz.gpg
#
#   # list what is available without touching anything:
#   deploy/restore.sh --list
#
#   # restore the database only, into an empty database:
#   deploy/restore.sh --stamp <s> --db-only --dsn postgres://.../jiuyue_verify
#
# Options: --stamp, --db, --files, --from, --dsn, --files-dir, --db-only,
#          --files-only, --clean, --no-create-db, --maintenance-db, --owner,
#          --passphrase-file, --list, --help.
#
# Environment (see deploy/env/backup.env.example): BACKUP_DIR, DATA_DIR,
# GPG_PASSPHRASE_FILE, BACKUP_REMOTE, ENV_FILE, PG_RESTORE, PSQL.

set -Eeuo pipefail
IFS=$'\n\t'

BACKUP_DIR="${BACKUP_DIR:-/var/backups/jiuyue}"
DATA_DIR="${DATA_DIR:-/var/lib/jiuyue}"
ENV_FILE="${ENV_FILE:-/etc/jiuyue/jiuyue.env}"
GPG_PASSPHRASE_FILE="${GPG_PASSPHRASE_FILE:-/etc/jiuyue/backup_gpg_pass}"
BACKUP_REMOTE="${BACKUP_REMOTE:-}"

PG_RESTORE="${PG_RESTORE:-pg_restore}"
PSQL="${PSQL:-psql}"

STAMP=""
DB_ARTIFACT=""
FILES_ARTIFACT=""
FROM=""
DSN=""
FILES_DIR="${DATA_DIR}"
MAINTENANCE_DB="postgres"
RESTORE_OWNER=""
DB_ONLY=0
FILES_ONLY=0
CLEAN=0
CREATE_DB=1
LIST=0

WORK_DIR=""

log() { printf '%s [restore] %s\n' "$(date -u +%FT%TZ)" "$*" >&2; }
die() { printf '%s [restore] ERROR: %s\n' "$(date -u +%FT%TZ)" "$*" >&2; exit 1; }

usage() {
  sed -n '3,34p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit "${1:-0}"
}

cleanup() {
  local code=$?
  trap - EXIT
  [ -n "$WORK_DIR" ] && [ -d "$WORK_DIR" ] && rm -rf -- "$WORK_DIR"
  exit "$code"
}
trap cleanup EXIT

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --stamp)          STAMP="${2:-}"; shift 2 ;;
      --db)             DB_ARTIFACT="${2:-}"; shift 2 ;;
      --files)          FILES_ARTIFACT="${2:-}"; shift 2 ;;
      --from)           FROM="${2:-}"; shift 2 ;;
      --dsn)            DSN="${2:-}"; shift 2 ;;
      --files-dir)      FILES_DIR="${2:-}"; shift 2 ;;
      --maintenance-db) MAINTENANCE_DB="${2:-}"; shift 2 ;;
      --owner)          RESTORE_OWNER="${2:-}"; shift 2 ;;
      --passphrase-file) GPG_PASSPHRASE_FILE="${2:-}"; shift 2 ;;
      --db-only)        DB_ONLY=1; shift ;;
      --files-only)     FILES_ONLY=1; shift ;;
      --clean)          CLEAN=1; shift ;;
      --no-create-db)   CREATE_DB=0; shift ;;
      --list)           LIST=1; shift ;;
      -h|--help)        usage 0 ;;
      *)                die "unknown argument: $1 (see --help)" ;;
    esac
  done
  [ "$DB_ONLY" -eq 1 ] && [ "$FILES_ONLY" -eq 1 ] && die "--db-only and --files-only are mutually exclusive"
  if [ "$LIST" -eq 0 ]; then
    [ -n "$STAMP" ] || [ -n "$DB_ARTIFACT" ] || [ -n "$FILES_ARTIFACT" ] \
      || die "give --stamp (or --db/--files); see --help"
    if [ -n "$STAMP" ] && { [ -n "$DB_ARTIFACT" ] || [ -n "$FILES_ARTIFACT" ]; }; then
      die "give either --stamp or explicit --db/--files, not both"
    fi
  fi
}

need_cmd() { command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"; }

is_rclone_remote() {
  case "$1" in
    /*|./*|../*) return 1 ;;
    *:*)         return 0 ;;
    *)           return 1 ;;
  esac
}

gpg_decrypt() {
  gpg --batch --yes --quiet --pinentry-mode loopback \
      --passphrase-file "$GPG_PASSPHRASE_FILE" -d
}

resolve_defaults() {
  if [ -z "$DSN" ] && [ -f "$ENV_FILE" ]; then
    DSN="$(grep -m1 '^DATABASE_URL=' "$ENV_FILE" | cut -d= -f2-)" || true
  fi
  if [ -z "$FROM" ]; then
    if [ -n "$BACKUP_REMOTE" ]; then FROM="$BACKUP_REMOTE"; else FROM="$BACKUP_DIR"; fi
  fi
}

# Fetch a single file from the source (local dir or rclone remote) into $2.
fetch_file() {
  local name="$1" dest="$2"
  if is_rclone_remote "$FROM"; then
    rclone copyto --retries 3 "${FROM%/}/${name}" "$dest" \
      || die "could not download ${name} from ${FROM}"
  else
    [ -f "${FROM%/}/${name}" ] || die "backup file not found: ${FROM%/}/${name}"
    cp -p -- "${FROM%/}/${name}" "$dest"
  fi
}

list_sets() {
  log "backup sets available in ${FROM}"
  if is_rclone_remote "$FROM"; then
    rclone lsf --include 'jiuyue-*.manifest' "$FROM" 2>/dev/null | sed 's/\.manifest$//'
  else
    [ -d "$FROM" ] || die "source directory not found: ${FROM}"
    find "$FROM" -maxdepth 1 -type f -name 'jiuyue-*.manifest' -printf '%f\n' \
      | sed 's/\.manifest$//' | sort
  fi
}

# sha256-or-refuse: the sidecar must exist and match, or there is no restore.
verify_sidecar() {
  local artifact="$1" dir sidecar
  dir="$(dirname -- "$artifact")"
  sidecar="${artifact}.sha256"
  if [ ! -f "$sidecar" ]; then
    fetch_file "$(basename "$sidecar")" "${dir}/$(basename "$sidecar")" 2>/dev/null || true
  fi
  [ -f "$sidecar" ] || die "checksum sidecar missing: ${sidecar}; refusing to restore an unverified archive"
  ( cd "$dir" && sha256sum -c "$(basename "$sidecar")" >/dev/null ) \
    || die "sha256 mismatch for $(basename "$artifact") — the archive is corrupt or tampered with"
  log "sha256 verified: $(basename "$artifact")"
}

dbname_from_dsn() {
  printf '%s' "$1" | sed -E 's#^.*/([^/?]+)(\?.*)?$#\1#'
}

maintenance_dsn_from() {
  printf '%s' "$1" | sed -E 's#^(.*://[^/]*)/[^/?]*(.*)$#\1/'"${MAINTENANCE_DB}"'\2#'
}

ensure_database() {
  local dsn="$1" dbname admin exists
  dbname="$(dbname_from_dsn "$dsn")"
  admin="$(maintenance_dsn_from "$dsn")"
  if [ "$admin" = "$dsn" ]; then
    die "could not derive a maintenance connection from the DSN; pass a DSN that names a database"
  fi
  exists="$("$PSQL" -X -tA -c "SELECT 1 FROM pg_database WHERE datname='${dbname}'" "$admin" 2>/dev/null | tr -d '[:space:]')"
  if [ "$exists" = "1" ]; then
    log "target database exists: ${dbname}"
    return 0
  fi
  [ "$CREATE_DB" -eq 1 ] || die "target database ${dbname} does not exist and --no-create-db was given"
  log "creating target database: ${dbname}"
  "$PSQL" -X -v ON_ERROR_STOP=1 -q -c "CREATE DATABASE \"${dbname}\"" "$admin" \
    || die "could not create database ${dbname} (need CREATEDB, or create it as the postgres superuser)"
}

table_exists() {
  # libpq convention: options first, database name last (also what works under
  # MSYS on Windows).
  local dsn="$1" table="$2"
  "$PSQL" -X -tA -c \
    "SELECT 1 FROM information_schema.tables WHERE table_schema='public' AND table_name='${table}'" \
    "$dsn" 2>/dev/null | tr -d '[:space:]'
}

restore_database() {
  local artifact="$1"
  if [ "$CLEAN" -eq 0 ] && [ "$(table_exists "$DSN" users)" = "1" ]; then
    die "target database already contains a 'users' table; pass --clean to replace it, or restore into an empty database"
  fi
  local -a flags=( --no-owner --no-privileges )
  if [ "$CLEAN" -eq 1 ]; then
    flags+=( --clean --if-exists )
  fi
  log "restoring database into ${DSN}"
  gpg_decrypt < "$artifact" | gunzip | "$PG_RESTORE" -d "$DSN" "${flags[@]}" \
    || die "pg_restore failed (the database may be partially restored — inspect it)"
  log "database restored"
}

restore_files() {
  local artifact="$1"
  mkdir -p -- "$FILES_DIR"
  if ! chmod 0750 -- "$FILES_DIR" 2>/dev/null; then
    log "note: could not set mode 0750 on ${FILES_DIR} (continuing)"
  fi
  log "restoring the file store into ${FILES_DIR}"
  gpg_decrypt < "$artifact" | tar -xzf - -C "$FILES_DIR" \
    || die "tar extraction failed (the file store may be partially restored)"
  if [ -n "$RESTORE_OWNER" ] && id -u "${RESTORE_OWNER%%:*}" >/dev/null 2>&1; then
    chown -R -- "$RESTORE_OWNER" "$FILES_DIR"
  fi
  log "file store restored ($(find "$FILES_DIR" -type f | wc -l | tr -d ' ') files)"
}

print_summary() {
  # Only meaningful when a database was part of this restore.
  [ -n "$DSN" ] || return 0
  command -v "$PSQL" >/dev/null 2>&1 || return 0
  log "row counts after restore:"
  local t
  for t in users sessions conversations conversation_members messages; do
    local n
    n="$("$PSQL" -X -tA -c "SELECT count(*) FROM ${t}" "$DSN" 2>/dev/null | tr -d '[:space:]')"
    [ -n "$n" ] && printf '  %-22s %s\n' "$t" "$n" >&2
  done
}

main() {
  parse_args "$@"
  need_cmd gpg
  need_cmd gunzip
  need_cmd tar
  need_cmd sha256sum

  resolve_defaults
  if is_rclone_remote "$FROM"; then
    need_cmd rclone
  fi

  # Listing needs no passphrase: an operator may well want to see which sets are
  # available before fetching the key from the password manager.
  if [ "$LIST" -eq 1 ]; then
    list_sets
    return 0
  fi

  [ -f "$GPG_PASSPHRASE_FILE" ] \
    || die "GPG passphrase file not found: ${GPG_PASSPHRASE_FILE}"

  WORK_DIR="$(mktemp -d)"

  if [ -n "$STAMP" ]; then
    local set_name="jiuyue-${STAMP}"
    DB_ARTIFACT="${FROM%/}/${set_name}.db.dump.gz.gpg"
    FILES_ARTIFACT="${FROM%/}/${set_name}.files.tar.gz.gpg"
    if is_rclone_remote "$FROM"; then
      fetch_file "${set_name}.db.dump.gz.gpg"    "${WORK_DIR}/${set_name}.db.dump.gz.gpg"
      fetch_file "${set_name}.db.dump.gz.gpg.sha256" "${WORK_DIR}/${set_name}.db.dump.gz.gpg.sha256"
      fetch_file "${set_name}.files.tar.gz.gpg"  "${WORK_DIR}/${set_name}.files.tar.gz.gpg"
      fetch_file "${set_name}.files.tar.gz.gpg.sha256" "${WORK_DIR}/${set_name}.files.tar.gz.gpg.sha256"
      DB_ARTIFACT="${WORK_DIR}/${set_name}.db.dump.gz.gpg"
      FILES_ARTIFACT="${WORK_DIR}/${set_name}.files.tar.gz.gpg"
    fi
  fi

  need_cmd "$PG_RESTORE"

  if [ "$FILES_ONLY" -eq 0 ]; then
    [ -n "$DB_ARTIFACT" ] || die "--db artifact was not resolved"
    verify_sidecar "$DB_ARTIFACT"
  fi
  if [ "$DB_ONLY" -eq 0 ]; then
    [ -n "$FILES_ARTIFACT" ] || die "--files artifact was not resolved"
    verify_sidecar "$FILES_ARTIFACT"
  fi

  if [ "$FILES_ONLY" -eq 0 ]; then
    need_cmd "$PSQL"
    ensure_database "$DSN"
    restore_database "$DB_ARTIFACT"
  fi
  if [ "$DB_ONLY" -eq 0 ]; then
    restore_files "$FILES_ARTIFACT"
  fi

  print_summary
  log "restore complete"
}

main "$@"
