#!/usr/bin/env bash
#
# backup.sh — produce ONE restorable, encrypted backup set and ship it off the box.
#
# A backup nobody has restored from is a hypothesis, not a backup. That is why
# this script does four things and refuses to fake any of them:
#
#   1. dumps PostgreSQL (`pg_dump -Fc`) and archives the file store
#      (`/var/lib/jiuyue`, ADR-0006) — the database is not the whole system;
#   2. ENCRYPTS BOTH before either leaves the machine (GPG symmetric, AES-256,
#      passphrase read from a file that lives outside the backup);
#   3. VERIFIES both artifacts are readable before upload: the database artifact
#      must be a valid `pg_restore` archive and the file artifact a valid tar.
#      An artifact that cannot be read back is deleted and the run fails;
#   4. uploads the set and applies TWO INDEPENDENT retention policies:
#        * local  — keep BACKUP_KEEP_LOCAL_DAYS  on the box
#        * remote — keep BACKUP_KEEP_REMOTE_DAYS off the box
#      Upload is `copy`, never `sync`, so a local prune can never reach back and
#      delete remote history. A local disk failure must not destroy the off-box
#      copies — that is the entire point of keeping them apart.
#
# Alerting is heartbeat-style: every run pings HEARTBEAT_URL with status=up, and
# ANY non-zero exit pings status=down. The dangerous case is not a failed backup,
# it is a backup that never ran: an Uptime Kuma push monitor (or equivalent) is
# configured to expect the ping, so silence itself raises the alert.
#
# No containers anywhere (ADR-0010): native `pg_dump`, native `tar`, native `gpg`.
#
# Usage:
#   sudo deploy/backup.sh
#   sudo deploy/backup.sh --date 20260917T003000Z     # deterministic stamp (tests)
#   sudo deploy/backup.sh --no-prune                  # keep everything, this once
#
# Configuration comes from the environment (see deploy/env/backup.env.example);
# the systemd unit loads it from /etc/jiuyue/backup.env. Everything is
# overridable, so the whole thing can run against temporary directories.
#
# Backup set layout under ${BACKUP_DIR} (and mirrored to ${BACKUP_REMOTE}):
#   jiuyue-<stamp>.db.dump.gz.gpg          encrypted PostgreSQL custom-format dump
#   jiuyue-<stamp>.db.dump.gz.gpg.sha256   its checksum sidecar (sha256sum format)
#   jiuyue-<stamp>.files.tar.gz.gpg        encrypted archive of the file store
#   jiuyue-<stamp>.files.tar.gz.gpg.sha256 its checksum sidecar
#   jiuyue-<stamp>.manifest                metadata + checksums (no secrets)
#
# The `.sha256` sidecar is the same artifact/sidecar convention as
# deploy/deploy.sh and .github/workflows/release.yml. restore.sh verifies it and
# REFUSES to restore an archive whose checksum does not match.

set -Eeuo pipefail
IFS=$'\n\t'

# --- paths and names (override via environment for tests) ------------------
BACKUP_DIR="${BACKUP_DIR:-/var/backups/jiuyue}"
DATA_DIR="${DATA_DIR:-/var/lib/jiuyue}"
ENV_FILE="${ENV_FILE:-/etc/jiuyue/jiuyue.env}"
GPG_PASSPHRASE_FILE="${GPG_PASSPHRASE_FILE:-/etc/jiuyue/backup_gpg_pass}"
BACKUP_REMOTE="${BACKUP_REMOTE:-}"          # a directory path, or an rclone remote ("r2crypt:jiuyue")
HEARTBEAT_URL="${HEARTBEAT_URL:-}"          # Uptime Kuma push URL; empty = no alerting
BACKUP_KEEP_LOCAL_DAYS="${BACKUP_KEEP_LOCAL_DAYS:-7}"
BACKUP_KEEP_REMOTE_DAYS="${BACKUP_KEEP_REMOTE_DAYS:-30}"
BACKUP_LOCK="${BACKUP_LOCK:-/run/lock/jiuyue-backup.lock}"

PG_DUMP="${PG_DUMP:-pg_dump}"
PG_RESTORE="${PG_RESTORE:-pg_restore}"

DATE_STAMP=""
PRUNE=1
UPLOAD=1

STAGE_DIR=""

log() { printf '%s [backup] %s\n' "$(date -u +%FT%TZ)" "$*" >&2; }
die() { printf '%s [backup] ERROR: %s\n' "$(date -u +%FT%TZ)" "$*" >&2; exit 1; }

usage() {
  sed -n '3,55p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit "${1:-0}"
}

# A single EXIT trap so cleanup and the failure heartbeat can never be skipped,
# however the script exits.
cleanup() {
  local code=$?
  trap - EXIT
  if [ -n "$STAGE_DIR" ] && [ -d "$STAGE_DIR" ]; then
    rm -rf -- "$STAGE_DIR"
  fi
  if [ "$code" -ne 0 ]; then
    heartbeat down "backup failed (exit ${code})"
  fi
  exit "$code"
}
trap cleanup EXIT

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --date)      DATE_STAMP="${2:-}"; shift 2 ;;
      --no-prune)  PRUNE=0; shift ;;
      --no-upload) UPLOAD=0; shift ;;   # tests only: skips upload AND remote retention
      -h|--help)   usage 0 ;;
      *)           die "unknown argument: $1 (see --help)" ;;
    esac
  done
  [[ "$BACKUP_KEEP_LOCAL_DAYS" =~ ^[0-9]+$ ]]  || die "BACKUP_KEEP_LOCAL_DAYS must be an integer"
  [[ "$BACKUP_KEEP_REMOTE_DAYS" =~ ^[0-9]+$ ]] || die "BACKUP_KEEP_REMOTE_DAYS must be an integer"
  if [ -n "$DATE_STAMP" ]; then
    [[ "$DATE_STAMP" =~ ^[A-Za-z0-9._-]+$ ]] || die "--date must be [A-Za-z0-9._-]+"
  fi
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

# mkdir + a best-effort mode. `chmod` can legitimately be refused (a mounted
# share, a restricted filesystem, MSYS), and that must not sink a backup.
ensure_dir() {
  local dir="$1" mode="${2:-0750}"
  mkdir -p -- "$dir"
  if ! chmod "$mode" -- "$dir" 2>/dev/null; then
    log "note: could not set mode ${mode} on ${dir} (continuing)"
  fi
}

# Best-effort: a heartbeat failure must never mask or replace the real outcome.
heartbeat() {
  local status="$1" msg="${2:-}"
  if [ -z "$HEARTBEAT_URL" ]; then
    if [ "$status" = down ]; then
      log "ERROR: no HEARTBEAT_URL configured — this failure cannot alert anyone"
    else
      log "WARNING: no HEARTBEAT_URL configured; the run is not being watched"
    fi
    return 0
  fi
  if curl -fsS -G --max-time 10 \
       --data-urlencode "status=${status}" \
       --data-urlencode "msg=${msg}" \
       "$HEARTBEAT_URL" >/dev/null 2>&1; then
    log "heartbeat sent: status=${status}"
  else
    log "WARNING: heartbeat ping failed (status=${status}) — monitor will notice on its own"
  fi
  return 0
}

# An rclone remote is "name:path"; a local target is a path. Absolute paths and
# explicit relative paths never contain a colon we care about.
is_rclone_remote() {
  case "$1" in
    /*|./*|../*) return 1 ;;
    *:*)         return 0 ;;
    *)           return 1 ;;
  esac
}

resolve_database_url() {
  if [ -z "${DATABASE_URL:-}" ] && [ -f "$ENV_FILE" ]; then
    DATABASE_URL="$(grep -m1 '^DATABASE_URL=' "$ENV_FILE" | cut -d= -f2-)" || true
  fi
  [ -n "${DATABASE_URL:-}" ] \
    || die "no DATABASE_URL in the environment or ${ENV_FILE}"
}

# GPG symmetric encryption: content and the single artifact file are both
# protected; nothing leaves the box in the clear.
gpg_encrypt() {
  gpg --batch --yes --quiet --pinentry-mode loopback \
      --passphrase-file "$GPG_PASSPHRASE_FILE" \
      --cipher-algo AES256 -c
}

gpg_decrypt() {
  gpg --batch --yes --quiet --pinentry-mode loopback \
      --passphrase-file "$GPG_PASSPHRASE_FILE" -d
}

build_database_artifact() {
  local out="$1"
  log "dumping PostgreSQL (custom format, piped end-to-end)"
  "$PG_DUMP" -Fc --no-owner --no-privileges "$DATABASE_URL" \
    | gzip -6 \
    | gpg_encrypt > "$out" \
    || die "database dump failed (pg_dump / gzip / gpg)"
}

build_files_artifact() {
  local out="$1"
  log "archiving the file store at ${DATA_DIR}"
  tar -czf - -C "$DATA_DIR" . \
    | gpg_encrypt > "$out" \
    || die "file-store archive failed (tar / gpg)"
}

# Refuse to ship something we cannot read back. This is the cheap half of the
# "restorable" claim: it proves the archive decrypts and parses, not that the
# data is correct — restore-drill.sh proves that.
verify_artifacts() {
  local db_artifact="$1" files_artifact="$2"
  gpg_decrypt < "$db_artifact" | gunzip | "$PG_RESTORE" --list >/dev/null 2>&1 \
    || die "database artifact is not a readable pg_restore archive: ${db_artifact}"
  gpg_decrypt < "$files_artifact" | tar -tzf - >/dev/null 2>&1 \
    || die "file artifact is not a readable tar.gz: ${files_artifact}"
  log "both artifacts decrypt and parse (pg_restore --list / tar -tzf)"
}

# sha256sum sidecars, one per encrypted artifact, written with bare basenames so
# the sidecar is valid whatever directory restore.sh runs from.
write_checksums() {
  local dir="$1" db_name="$2" files_name="$3"
  ( cd "$dir" \
      && sha256sum "$db_name"    > "${db_name}.sha256" \
      && sha256sum "$files_name" > "${files_name}.sha256" )
}

write_manifest() {
  local dir="$1" set_name="$2" stamp="$3" db_name="$4" files_name="$5"
  local db_sha files_sha file_count pg_version
  db_sha="$(cd "$dir" && awk 'NR==1{print $1}' "${db_name}.sha256")"
  files_sha="$(cd "$dir" && awk 'NR==1{print $1}' "${files_name}.sha256")"
  file_count="$(find "$DATA_DIR" -type f 2>/dev/null | wc -l | tr -d ' ')"
  pg_version="$("$PG_DUMP" --version 2>/dev/null | tr -d '\r')"
  {
    printf 'set=%s\n' "$set_name"
    printf 'stamp=%s\n' "$stamp"
    printf 'created_at=%s\n' "$(date -u +%FT%TZ)"
    printf 'host=%s\n' "$(hostname 2>/dev/null || echo unknown)"
    printf 'database_url_database=%s\n' "$(printf '%s' "$DATABASE_URL" | sed 's#?.*##; s#.*/##')"
    printf 'pg_dump=%s\n' "$pg_version"
    printf 'data_dir=%s\n' "$DATA_DIR"
    printf 'file_count=%s\n' "$file_count"
    printf 'encryption=gpg-symmetric-aes256\n'
    printf 'db_artifact=%s\n' "$db_name"
    printf 'db_bytes=%s\n' "$(cd "$dir" && wc -c < "$db_name" | tr -d ' ')"
    printf 'db_sha256=%s\n' "$db_sha"
    printf 'files_artifact=%s\n' "$files_name"
    printf 'files_bytes=%s\n' "$(cd "$dir" && wc -c < "$files_name" | tr -d ' ')"
    printf 'files_sha256=%s\n' "$files_sha"
  } > "${dir}/${set_name}.manifest"
}

upload_set() {
  local dir="$1"; shift
  if [ "$UPLOAD" -eq 0 ]; then
    log "upload skipped (--no-upload)"
    return 0
  fi
  [ -n "$BACKUP_REMOTE" ] \
    || die "BACKUP_REMOTE is not configured; refusing to produce a backup that never leaves the box"

  if is_rclone_remote "$BACKUP_REMOTE"; then
    need_cmd rclone
    local f base
    for f in "$@"; do
      base="$(basename "$f")"
      log "uploading ${base} -> ${BACKUP_REMOTE}"
      rclone copyto --transfers=2 --checkers=4 --retries 3 \
        "$f" "${BACKUP_REMOTE%/}/${base}" \
        || die "rclone copyto failed for ${base}"
    done
  else
    ensure_dir "$BACKUP_REMOTE" 0750
    local f base
    for f in "$@"; do
      base="$(basename "$f")"
      cp -p -- "$f" "${BACKUP_REMOTE%/}/${base}" \
        || die "copy to remote directory failed for ${base}"
    done
    log "uploaded backup set to ${BACKUP_REMOTE}"
  fi
}

prune_local() {
  local days="$1"
  log "local retention: deleting sets older than ${days} days from ${BACKUP_DIR}"
  find "$BACKUP_DIR" -maxdepth 1 -type f -name 'jiuyue-*' -mtime "+${days}" -delete
}

# Deliberately separate from prune_local: different directory (or different
# system entirely), different age, and never `sync` — a local failure cannot
# delete remote history.
prune_remote() {
  local days="$1"
  if [ "$UPLOAD" -eq 0 ] || [ -z "$BACKUP_REMOTE" ]; then
    return 0
  fi
  log "remote retention: deleting sets older than ${days} days from ${BACKUP_REMOTE}"
  if is_rclone_remote "$BACKUP_REMOTE"; then
    rclone delete --min-age "${days}d" --include 'jiuyue-*' "$BACKUP_REMOTE" \
      || log "WARNING: remote retention on ${BACKUP_REMOTE} failed (not fatal)"
  else
    find "$BACKUP_REMOTE" -maxdepth 1 -type f -name 'jiuyue-*' -mtime "+${days}" -delete
  fi
}

main() {
  parse_args "$@"

  need_cmd "$PG_DUMP"
  need_cmd "$PG_RESTORE"
  need_cmd gpg
  need_cmd gzip
  need_cmd tar
  need_cmd sha256sum
  need_cmd curl
  need_cmd find

  [ -f "$GPG_PASSPHRASE_FILE" ] \
    || die "GPG passphrase file not found: ${GPG_PASSPHRASE_FILE} (see docs/backup.md)"
  [ -d "$DATA_DIR" ] \
    || die "file store directory not found: ${DATA_DIR}"
  resolve_database_url

  mkdir -p -- "$BACKUP_DIR"
  # Serialise runs on the same box. `flock` is absent on some dev platforms
  # (MSYS), so it is used only when present and when a lock path is configured.
  if command -v flock >/dev/null 2>&1 && [ -n "$BACKUP_LOCK" ]; then
    mkdir -p -- "$(dirname -- "$BACKUP_LOCK")" 2>/dev/null || true
    if exec 9>"$BACKUP_LOCK"; then
      flock -n 9 || die "another backup appears to be in progress (${BACKUP_LOCK})"
    else
      log "WARNING: could not open the lock file ${BACKUP_LOCK}; continuing without serialisation"
    fi
  fi

  local stamp set_name
  stamp="${DATE_STAMP:-$(date -u +%Y%m%dT%H%M%SZ)}"
  set_name="jiuyue-${stamp}"
  local db_name="${set_name}.db.dump.gz.gpg"
  local files_name="${set_name}.files.tar.gz.gpg"

  # Stage next to the final location so the publish move is a rename on the same
  # filesystem (deploy.sh does the same for releases).
  STAGE_DIR="${BACKUP_DIR}/.staging.${stamp}.$$"
  mkdir -p -- "$STAGE_DIR"

  build_database_artifact  "${STAGE_DIR}/${db_name}"
  build_files_artifact     "${STAGE_DIR}/${files_name}"
  verify_artifacts         "${STAGE_DIR}/${db_name}" "${STAGE_DIR}/${files_name}"
  write_checksums          "$STAGE_DIR" "$db_name" "$files_name"
  write_manifest           "$STAGE_DIR" "$set_name" "$stamp" "$db_name" "$files_name"

  # Publish the staged set into BACKUP_DIR, replacing a same-stamp set cleanly.
  rm -f -- "${BACKUP_DIR}/${db_name}" "${BACKUP_DIR}/${db_name}.sha256" \
            "${BACKUP_DIR}/${files_name}" "${BACKUP_DIR}/${files_name}.sha256" \
            "${BACKUP_DIR}/${set_name}.manifest"
  mv -- "${STAGE_DIR}/${db_name}"            "${BACKUP_DIR}/"
  mv -- "${STAGE_DIR}/${db_name}.sha256"     "${BACKUP_DIR}/"
  mv -- "${STAGE_DIR}/${files_name}"         "${BACKUP_DIR}/"
  mv -- "${STAGE_DIR}/${files_name}.sha256"  "${BACKUP_DIR}/"
  mv -- "${STAGE_DIR}/${set_name}.manifest"  "${BACKUP_DIR}/"
  rm -rf -- "$STAGE_DIR"; STAGE_DIR=""

  log "backup set ready: ${BACKUP_DIR}/${set_name}.{db.dump.gz.gpg,files.tar.gz.gpg,manifest}"

  upload_set "$BACKUP_DIR" \
    "${BACKUP_DIR}/${db_name}" "${BACKUP_DIR}/${db_name}.sha256" \
    "${BACKUP_DIR}/${files_name}" "${BACKUP_DIR}/${files_name}.sha256" \
    "${BACKUP_DIR}/${set_name}.manifest"

  if [ "$PRUNE" -eq 1 ]; then
    prune_local  "$BACKUP_KEEP_LOCAL_DAYS"
    prune_remote "$BACKUP_KEEP_REMOTE_DAYS"
  else
    log "pruning skipped (--no-prune)"
  fi

  heartbeat up "backup ${set_name} ok"
  log "done: ${set_name}"
}

main "$@"
