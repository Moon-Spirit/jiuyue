#!/usr/bin/env bash
#
# deploy.sh — install and activate a jiuyue release. One entry point, idempotent.
#
# What it does, in order:
#   1. acquires a lock (no two deploys at once)
#   2. fetches the release artifact (local path or URL) and verifies its sha256
#   3. unpacks it into /opt/jiuyue/releases/<stamp>/
#   4. atomically repoints /opt/jiuyue/current at the new release
#   5. restarts the `jiuyue` systemd service
#   6. health-checks http://127.0.0.1:8080/health
#   7. on failure: repoints `current` at the previous release, restarts, and
#      re-checks — the service is never left half-updated
#
# Migrations are NOT run as a separate step. They are embedded in the binary
# (backend/crates/store/src/store.rs, `sqlx::migrate!`) and applied at server
# startup, before it accepts traffic. Rolling the binary back does NOT roll the
# schema back, so migrations must stay backward-compatible for one release
# (expand/contract). See docs/deploy.md.
#
# No containers anywhere (ADR-0010): this is a plain file/symlink/systemd deploy.
#
# Usage:
#   sudo deploy/deploy.sh --artifact ./jiuyue-0.1.0-linux-x86_64.tar.gz \
#                         --sha256 <64-hex>
#   sudo deploy/deploy.sh --artifact https://.../jiuyue-0.1.0-linux-x86_64.tar.gz
#   sudo deploy/deploy.sh --artifact <path> --public-url https://chat.example.com/api/health
#
# Artifact layout (the interface owned by the CI publish job, ticket #44):
#   bin/jiuyue-server     the release binary
#   dist/                 the Vite production build (frontend/dist)
# An optional `VERSION` file is copied along but not interpreted.

set -Eeuo pipefail
IFS=$'\n\t'

# --- paths and names (override via environment for tests) ------------------
APP_ROOT="${APP_ROOT:-/opt/jiuyue}"
RELEASES_DIR="${RELEASES_DIR:-${APP_ROOT}/releases}"
CURRENT_LINK="${CURRENT_LINK:-${APP_ROOT}/current}"
DATA_DIR="${DATA_DIR:-/var/lib/jiuyue}"
ENV_FILE="${ENV_FILE:-/etc/jiuyue/jiuyue.env}"
SERVICE="${SERVICE:-jiuyue}"
LOCK_FILE="${LOCK_FILE:-/run/lock/jiuyue-deploy.lock}"
HEALTH_URL="${HEALTH_URL:-http://127.0.0.1:8080/health}"
HEALTH_TIMEOUT="${HEALTH_TIMEOUT:-60}"
KEEP_RELEASES="${KEEP_RELEASES:-3}"

ARTIFACT=""
SHA256=""
VERSION=""
PUBLIC_URL=""
PRUNE=1

TMP_DIR=""
STAGE_DIR=""

log()  { printf '%s [deploy] %s\n' "$(date -u +%FT%TZ)" "$*" >&2; }
die()  { printf '%s [deploy] ERROR: %s\n' "$(date -u +%FT%TZ)" "$*" >&2; exit 1; }

usage() {
  sed -n '3,40p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit "${1:-0}"
}

cleanup() {
  if [ -n "$TMP_DIR" ] && [ -d "$TMP_DIR" ]; then
    rm -rf -- "$TMP_DIR"
  fi
  if [ -n "$STAGE_DIR" ] && [ -d "$STAGE_DIR" ]; then
    rm -rf -- "$STAGE_DIR"
  fi
}

on_error() {
  local code="$1"
  log "aborted at line ${BASH_LINENO[0]} (exit ${code})"
}

trap cleanup EXIT
trap 'on_error $?' ERR

need_root() {
  if [ "$(id -u)" -ne 0 ]; then
    die "must run as root (try: sudo $0 ...)"
  fi
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --artifact)   ARTIFACT="${2:-}"; shift 2 ;;
      --sha256)     SHA256="${2:-}"; shift 2 ;;
      --version)    VERSION="${2:-}"; shift 2 ;;
      --health-url) HEALTH_URL="${2:-}"; shift 2 ;;
      --health-timeout) HEALTH_TIMEOUT="${2:-}"; shift 2 ;;
      --public-url) PUBLIC_URL="${2:-}"; shift 2 ;;
      --keep)       KEEP_RELEASES="${2:-}"; shift 2 ;;
      --no-prune)   PRUNE=0; shift ;;
      -h|--help)    usage 0 ;;
      *)            die "unknown argument: $1 (see --help)" ;;
    esac
  done
  [ -n "$ARTIFACT" ] || die "--artifact is required (see --help)"
  [[ "$HEALTH_TIMEOUT" =~ ^[0-9]+$ ]] || die "--health-timeout must be seconds"
  [[ "$KEEP_RELEASES" =~ ^[0-9]+$ ]] || die "--keep must be an integer"
}

# Fetch text (the .sha256 sidecar) from a URL or a local file.
fetch_text() {
  local source="$1"
  case "$source" in
    http://*|https://*) curl -fLsS --retry 3 --retry-delay 2 "$source" ;;
    *)                  cat -- "$source" ;;
  esac
}

fetch_artifact() {
  local source="$1" dest="$2"
  case "$source" in
    http://*|https://*)
      log "downloading artifact: $source"
      curl -fLsS --retry 3 --retry-delay 2 -o "$dest" "$source"
      ;;
    *)
      [ -f "$source" ] || die "artifact not found: $source"
      cp -- "$source" "$dest"
      ;;
  esac
}

resolve_sha256() {
  if [ -n "$SHA256" ]; then
    return 0
  fi
  local sidecar="${ARTIFACT}.sha256"
  if ! SHA256="$(fetch_text "$sidecar" 2>/dev/null | awk 'NR==1{print $1}')"; then
    die "no --sha256 given and no readable checksum at ${sidecar}; refusing to deploy an unverified artifact"
  fi
  if [ -z "$SHA256" ]; then
    die "checksum sidecar ${sidecar} was empty; refusing to deploy an unverified artifact"
  fi
  log "using checksum from ${sidecar}"
}

verify_sha256() {
  local file="$1" expected
  expected="$(printf '%s' "$SHA256" | tr 'A-F' 'a-f')"
  [[ "$expected" =~ ^[0-9a-f]{64}$ ]] || die "checksum is not 64 hex characters: $SHA256"
  printf '%s  %s\n' "$expected" "$file" | sha256sum -c - >/dev/null \
    || die "sha256 mismatch — artifact is corrupt or tampered with"
  log "sha256 verified"
}

extract_and_validate() {
  local tarball="$1" stage="$2"
  mkdir -p -- "$stage"
  tar -xzf "$tarball" -C "$stage"
  [ -x "$stage/bin/jiuyue-server" ] || [ -f "$stage/bin/jiuyue-server" ] \
    || die "malformed artifact: bin/jiuyue-server is missing"
  [ -f "$stage/dist/index.html" ] \
    || die "malformed artifact: dist/index.html is missing"
}

install_release() {
  local stage="$1" final="$2"
  mkdir -p -- "$RELEASES_DIR"
  # A partly-written previous attempt at the same stamp must not linger.
  if [ -e "$final" ]; then
    log "replacing existing release directory ${final}"
    rm -rf -- "$final"
  fi
  mv -- "$stage" "$final"
  chown -R root:jiuyue "$final"
  find "$final" -type d -exec chmod 0755 {} +
  find "$final" -type f -exec chmod 0644 {} +
  chmod 0755 "$final/bin/jiuyue-server"
  log "installed release at ${final}"
}

switch_current() {
  local target="$1"
  ln -sfn -- "$target" "${CURRENT_LINK}.new"
  mv -Tf -- "${CURRENT_LINK}.new" "$CURRENT_LINK"
  log "current -> ${target}"
}

wait_healthy() {
  local url="$1" timeout="${2:-60}" i
  for (( i = 0; i < timeout; i++ )); do
    if curl -fsS --max-time 3 "$url" 2>/dev/null | grep -q '"status":"ok"'; then
      return 0
    fi
    sleep 1
  done
  return 1
}

rollback_to() {
  local previous="$1"
  log "health check failed — rolling back to ${previous}"
  switch_current "$previous"
  systemctl restart "$SERVICE" 9>&- || true
  if wait_healthy "$HEALTH_URL" "$HEALTH_TIMEOUT"; then
    log "rollback healthy; the box is serving the previous release again"
    return 0
  fi
  log "rollback did not become healthy either — operator attention required"
  return 1
}

prune_releases() {
  local keep="$1" current_target
  current_target="$(readlink -f -- "$CURRENT_LINK")"
  # Oldest first, never delete the active release.
  local -a stale=()
  while IFS= read -r dir; do
    [ "$dir" = "$current_target" ] && continue
    stale+=("$dir")
  done < <(find "$RELEASES_DIR" -mindepth 1 -maxdepth 1 -type d -printf '%T@ %p\n' \
             | sort -n | awk '{print $2}')
  local count=${#stale[@]}
  if [ "$count" -le "$keep" ]; then
    return 0
  fi
  local remove_count=$(( count - keep ))
  local i
  for (( i = 0; i < remove_count; i++ )); do
    log "pruning old release ${stale[$i]}"
    rm -rf -- "${stale[$i]}"
  done
}

main() {
  parse_args "$@"
  need_root
  need_cmd curl
  need_cmd sha256sum
  need_cmd tar
  need_cmd flock

  [ -f "$ENV_FILE" ] || die "environment file not found: ${ENV_FILE} (run deploy/provision.sh first)"
  [ -d "$DATA_DIR" ] || die "data directory not found: ${DATA_DIR} (run deploy/provision.sh first)"

  # The lock lives on fd 9 for the lifetime of this script. Bash does not set
  # FD_CLOEXEC on a descriptor opened this way, so **every child inherits it** -
  # and any child that outlives the deploy would hold the lock forever, making
  # every later deploy die with a misleading "another deploy appears to be in
  # progress". Real systemd starts the unit from PID 1 and would never inherit
  # it, but that is a property of systemd, not of this script. So the lock fd is
  # closed explicitly wherever a long-lived process can be spawned (`9>&-`).
  mkdir -p -- "$(dirname -- "$LOCK_FILE")"
  exec 9>"$LOCK_FILE"
  flock -n 9 || die "another deploy appears to be in progress (${LOCK_FILE})"

  local stamp="${VERSION:-$(date -u +%Y%m%dT%H%M%SZ)}"
  stamp="${stamp//[^A-Za-z0-9._-]/_}"
  local final="${RELEASES_DIR}/${stamp}"

  mkdir -p -- "$APP_ROOT" "$RELEASES_DIR"
  # Stage on the same filesystem as the final release so the final move is an
  # atomic rename, never a cross-device copy of a live directory.
  TMP_DIR="$(mktemp -d)"
  STAGE_DIR="${APP_ROOT}/.staging.${stamp}.$$"
  local tarball="${TMP_DIR}/artifact.tar.gz"
  local stage="${STAGE_DIR}"

  resolve_sha256
  fetch_artifact "$ARTIFACT" "$tarball"
  verify_sha256 "$tarball"
  extract_and_validate "$tarball" "$stage"

  local previous=""
  if [ -L "$CURRENT_LINK" ]; then
    previous="$(readlink -f -- "$CURRENT_LINK")"
  fi

  install_release "$stage" "$final"
  switch_current "$final"

  log "restarting ${SERVICE} (embedded migrations run at startup)"
  if ! systemctl restart "$SERVICE" 9>&-; then
    if [ -n "$previous" ] && [ -d "$previous" ]; then
      rollback_to "$previous"
    fi
    die "deploy failed: ${SERVICE} did not restart"
  fi

  if ! wait_healthy "$HEALTH_URL" "$HEALTH_TIMEOUT"; then
    if [ -n "$previous" ] && [ -d "$previous" ]; then
      rollback_to "$previous"
    else
      log "no previous release available; leaving ${stamp} in place for inspection"
    fi
    die "deploy failed: health check never passed at ${HEALTH_URL}"
  fi
  log "health check passed: ${HEALTH_URL}"

  if [ -n "$PUBLIC_URL" ]; then
    if wait_healthy "$PUBLIC_URL" "$HEALTH_TIMEOUT"; then
      log "public health check passed: ${PUBLIC_URL}"
    else
      log "WARNING: public health check failed at ${PUBLIC_URL} (app is up on loopback; check Caddy/DNS)"
    fi
  fi

  if [ "$PRUNE" -eq 1 ]; then
    prune_releases "$KEEP_RELEASES"
  fi

  log "deploy complete: release ${stamp}"
}

main "$@"
