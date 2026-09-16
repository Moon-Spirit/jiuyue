#!/usr/bin/env bash
#
# provision.sh — one-time, idempotent bootstrap of a fresh Debian/Ubuntu box
# for jiuyue. Run from a checkout of this repository:
#
#   sudo deploy/provision.sh --domain chat.example.com --acme-email ops@example.com
#
# It installs and configures, with no containers anywhere (ADR-0010):
#   * PostgreSQL 16 + the tuned jiuyue config
#   * the Caddy edge (official apt repository) + the Caddyfile
#   * the `jiuyue` service account, data directory and /etc/jiuyue secrets
#   * the systemd unit, memory caps and the Caddy env drop-in
#   * a 1 GB swap file and vm.swappiness=10
#
# It is safe to re-run: every step checks before it acts, and it never
# regenerates secrets that already exist in /etc/jiuyue/jiuyue.env.
#
# It does NOT deploy a release. After provisioning, install one with
# deploy/deploy.sh, or pass --artifact here to chain straight into it.
#
# The binary is never compiled on this box: it is pulled as a CI-built artifact.

set -Eeuo pipefail
IFS=$'\n\t'

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

PG_VERSION="${PG_VERSION:-16}"
DB_NAME="${DB_NAME:-jiuyue}"
DB_USER="${DB_USER:-jiuyue}"
DATA_DIR="${DATA_DIR:-/var/lib/jiuyue}"
APP_ROOT="${APP_ROOT:-/opt/jiuyue}"
ENV_FILE="${ENV_FILE:-/etc/jiuyue/jiuyue.env}"
CADDY_ENV_FILE="${CADDY_ENV_FILE:-/etc/jiuyue/caddy.env}"
BACKUP_ENV_FILE="${BACKUP_ENV_FILE:-/etc/jiuyue/backup.env}"
GPG_PASSPHRASE_FILE="${GPG_PASSPHRASE_FILE:-/etc/jiuyue/backup_gpg_pass}"
SWAP_FILE="${SWAP_FILE:-/swapfile}"
SWAP_SIZE_GB="${SWAP_SIZE_GB:-1}"

JIUYUE_DOMAIN="${JIUYUE_DOMAIN:-}"
JIUYUE_ACME_EMAIL="${JIUYUE_ACME_EMAIL:-}"
ARTIFACT=""
SHA256=""
MAKE_SWAP=1

log() { printf '%s [provision] %s\n' "$(date -u +%FT%TZ)" "$*" >&2; }
die() { printf '%s [provision] ERROR: %s\n' "$(date -u +%FT%TZ)" "$*" >&2; exit 1; }

usage() {
  sed -n '3,30p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit "${1:-0}"
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --domain)      JIUYUE_DOMAIN="${2:-}"; shift 2 ;;
      --acme-email)  JIUYUE_ACME_EMAIL="${2:-}"; shift 2 ;;
      --db-name)     DB_NAME="${2:-}"; shift 2 ;;
      --db-user)     DB_USER="${2:-}"; shift 2 ;;
      --swap-size-gb) SWAP_SIZE_GB="${2:-}"; shift 2 ;;
      --no-swap)     MAKE_SWAP=0; shift ;;
      --artifact)    ARTIFACT="${2:-}"; shift 2 ;;
      --sha256)      SHA256="${2:-}"; shift 2 ;;
      -h|--help)     usage 0 ;;
      *)             die "unknown argument: $1 (see --help)" ;;
    esac
  done
  [ "$(id -u)" -eq 0 ] || die "must run as root (try: sudo $0 ...)"
  [ -n "$JIUYUE_DOMAIN" ] || die "--domain is required (the public hostname Caddy serves)"
  [ -n "$JIUYUE_ACME_EMAIL" ] || die "--acme-email is required (Let's Encrypt account contact)"
  [[ "$SWAP_SIZE_GB" =~ ^[0-9]+$ ]] || die "--swap-size-gb must be an integer"
}

require_repo_assets() {
  local required=(
    "${SCRIPT_DIR}/systemd/jiuyue.service"
    "${SCRIPT_DIR}/systemd/drop-ins/postgresql@${PG_VERSION}-main.service.d/10-memory.conf"
    "${SCRIPT_DIR}/systemd/drop-ins/caddy.service.d/10-jiuyue-env.conf"
    "${SCRIPT_DIR}/postgresql/jiuyue-tuning.conf"
    "${SCRIPT_DIR}/env/jiuyue.env.example"
    "${SCRIPT_DIR}/Caddyfile"
    "${SCRIPT_DIR}/deploy.sh"
    "${SCRIPT_DIR}/backup.sh"
    "${SCRIPT_DIR}/restore.sh"
    "${SCRIPT_DIR}/restore-drill.sh"
    "${SCRIPT_DIR}/env/backup.env.example"
    "${SCRIPT_DIR}/systemd/jiuyue-backup.service"
    "${SCRIPT_DIR}/systemd/jiuyue-backup.timer"
    "${SCRIPT_DIR}/systemd/journald.conf.d/10-jiuyue-limits.conf"
    "${SCRIPT_DIR}/postgresql/jiuyue-logging.conf"
    "${SCRIPT_DIR}/logrotate/jiuyue-postgresql"
    "${SCRIPT_DIR}/monitor/monitor.sh"
    "${SCRIPT_DIR}/monitor/install-beszel.sh"
    "${SCRIPT_DIR}/env/monitor.env.example"
    "${SCRIPT_DIR}/env/beszel.env.example"
    "${SCRIPT_DIR}/systemd/jiuyue-monitor.service"
    "${SCRIPT_DIR}/systemd/jiuyue-monitor.timer"
    "${SCRIPT_DIR}/systemd/beszel-hub.service"
    "${SCRIPT_DIR}/systemd/beszel-agent.service"
  )
  local f
  for f in "${required[@]}"; do
    [ -f "$f" ] || die "missing repository asset: $f (run provision.sh from a full checkout)"
  done
}

add_caddy_repo() {
  if apt-cache policy caddy 2>/dev/null | grep -q 'Candidate: [^(]'; then
    log "caddy package already available"
    return 0
  fi
  log "adding the official Caddy apt repository"
  apt-get install -y debian-keyring debian-archive-keyring apt-transport-https curl gpg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
    | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
    > /etc/apt/sources.list.d/caddy-stable.list
  chmod o+r /usr/share/keyrings/caddy-stable-archive-keyring.gpg \
             /etc/apt/sources.list.d/caddy-stable.list
  apt-get update
}

install_packages() {
  log "installing packages (no containers)"
  export DEBIAN_FRONTEND=noninteractive
  apt-get update
  apt-get install -y \
    "postgresql-${PG_VERSION}" "postgresql-client-${PG_VERSION}" \
    caddy curl ca-certificates openssl util-linux \
    gnupg rclone
}

ensure_user_and_dirs() {
  if ! id -u "$DB_USER" >/dev/null 2>&1; then
    log "creating system user ${DB_USER}"
    useradd --system --no-create-home --home-dir "$DATA_DIR" \
            --shell /usr/sbin/nologin "$DB_USER"
  fi
  install -d -m 0750 -o "$DB_USER" -g "$DB_USER" "$DATA_DIR"
  install -d -m 0755 "$APP_ROOT" "$APP_ROOT/releases"
  install -d -m 0750 -o root -g "$DB_USER" /etc/jiuyue
}

ensure_swap() {
  if [ "$MAKE_SWAP" -eq 0 ]; then
    log "skipping swap setup (--no-swap)"
  elif swapon --show | grep -q .; then
    log "swap already active"
  else
    log "creating ${SWAP_SIZE_GB}G swap file at ${SWAP_FILE}"
    dd if=/dev/zero of="$SWAP_FILE" bs=1M count=$(( SWAP_SIZE_GB * 1024 )) status=none
    chmod 600 "$SWAP_FILE"
    mkswap "$SWAP_FILE" >/dev/null
    swapon "$SWAP_FILE"
    if ! grep -qs "^${SWAP_FILE} " /etc/fstab; then
      printf '%s none swap sw 0 0\n' "$SWAP_FILE" >> /etc/fstab
    fi
  fi

  log "writing vm.swappiness=10 and a larger accept backlog"
  {
    printf '# jiuyue: swap is an alarm buffer, not extra RAM; do not push OOM forward.\n'
    printf 'vm.swappiness = 10\n'
    printf '# WebSocket reconnect bursts must not be dropped by the accept queue.\n'
    printf 'net.core.somaxconn = 1024\n'
  } > /etc/sysctl.d/90-jiuyue.conf
  sysctl --system >/dev/null
}

ensure_pg_tuning() {
  local conf_dir="/etc/postgresql/${PG_VERSION}/main/conf.d"
  [ -d "$conf_dir" ] || die "PostgreSQL conf.d not found at ${conf_dir}; is postgresql-${PG_VERSION} installed?"
  log "installing PostgreSQL tuning to ${conf_dir}/20-jiuyue-tuning.conf"
  install -m 0644 "${SCRIPT_DIR}/postgresql/jiuyue-tuning.conf" \
                  "${conf_dir}/20-jiuyue-tuning.conf"
  systemctl enable postgresql
  systemctl restart "postgresql@${PG_VERSION}-main" 2>/dev/null \
    || systemctl restart postgresql
}

psql_pg() { runuser -u postgres -- psql "$@"; }

write_env_file() {
  if [ -f "$ENV_FILE" ]; then
    log "${ENV_FILE} exists; keeping existing secrets"
    return 0
  fi
  log "generating secrets and writing ${ENV_FILE}"
  local db_password jwt_secret
  db_password="$(openssl rand -hex 24)"
  jwt_secret="$(openssl rand -hex 32)"   # 64 chars >= the 32-byte minimum
  sed -e "s|REPLACE_ME_DB_PASSWORD|${db_password}|" \
      -e "s|REPLACE_ME_openssl_rand_base64_48|${jwt_secret}|" \
      "${SCRIPT_DIR}/env/jiuyue.env.example" > "$ENV_FILE"
  chown root:"$DB_USER" "$ENV_FILE"
  chmod 0640 "$ENV_FILE"
}

# Idempotently make the role and database match the password in ${ENV_FILE}.
# Runs on every provision so a partial earlier attempt self-heals.
ensure_database() {
  [ -f "$ENV_FILE" ] || die "environment file missing: ${ENV_FILE}"
  local dsn userinfo db_password exists
  dsn="$(grep -m1 '^DATABASE_URL=' "$ENV_FILE" | cut -d= -f2-)"
  userinfo="${dsn#*://}"
  userinfo="${userinfo%@*}"
  db_password="${userinfo#*:}"
  [ -n "$db_password" ] || die "could not parse a password from DATABASE_URL in ${ENV_FILE}"

  exists="$(psql_pg -tAc "SELECT 1 FROM pg_roles WHERE rolname='${DB_USER}'")"
  if [ "$exists" = "1" ]; then
    psql_pg -v ON_ERROR_STOP=1 -q -c "ALTER ROLE ${DB_USER} WITH LOGIN PASSWORD '${db_password}'"
  else
    log "creating role ${DB_USER}"
    psql_pg -v ON_ERROR_STOP=1 -q -c "CREATE ROLE ${DB_USER} LOGIN PASSWORD '${db_password}'"
  fi
  if ! psql_pg -tAc "SELECT 1 FROM pg_database WHERE datname='${DB_NAME}'" | grep -q 1; then
    log "creating database ${DB_NAME}"
    psql_pg -v ON_ERROR_STOP=1 -q -c "CREATE DATABASE ${DB_NAME} OWNER ${DB_USER}"
  fi
}

write_caddy_env() {
  log "writing ${CADDY_ENV_FILE}"
  {
    printf 'JIUYUE_DOMAIN=%s\n' "$JIUYUE_DOMAIN"
    printf 'JIUYUE_ACME_EMAIL=%s\n' "$JIUYUE_ACME_EMAIL"
  } > "$CADDY_ENV_FILE"
  chown root:caddy "$CADDY_ENV_FILE"
  chmod 0640 "$CADDY_ENV_FILE"
}

install_systemd_units() {
  log "installing systemd units and drop-ins"
  install -m 0644 "${SCRIPT_DIR}/systemd/jiuyue.service" /etc/systemd/system/jiuyue.service
  install -d /etc/systemd/system/postgresql@${PG_VERSION}-main.service.d
  install -m 0644 \
    "${SCRIPT_DIR}/systemd/drop-ins/postgresql@${PG_VERSION}-main.service.d/10-memory.conf" \
    /etc/systemd/system/postgresql@${PG_VERSION}-main.service.d/10-memory.conf
  install -d /etc/systemd/system/caddy.service.d
  install -m 0644 \
    "${SCRIPT_DIR}/systemd/drop-ins/caddy.service.d/10-jiuyue-env.conf" \
    /etc/systemd/system/caddy.service.d/10-jiuyue-env.conf
  systemctl daemon-reload
}

# Backup configuration and the encryption passphrase. Neither is a secret that
# belongs in git, and neither is ever echoed to the terminal.
ensure_backup_config() {
  install -d -m 0700 /var/backups/jiuyue
  install -d -m 0755 /usr/local/lib/jiuyue

  if [ -f "$BACKUP_ENV_FILE" ]; then
    log "${BACKUP_ENV_FILE} exists; keeping it"
  else
    log "writing ${BACKUP_ENV_FILE} from the template"
    install -m 0640 -o root -g root "${SCRIPT_DIR}/env/backup.env.example" "$BACKUP_ENV_FILE"
  fi

  if [ -f "$GPG_PASSPHRASE_FILE" ]; then
    log "${GPG_PASSPHRASE_FILE} exists; keeping the existing passphrase"
  else
    log "generating the backup encryption passphrase at ${GPG_PASSPHRASE_FILE}"
    ( umask 077; openssl rand -base64 48 > "$GPG_PASSPHRASE_FILE" )
    chmod 0600 "$GPG_PASSPHRASE_FILE"
    chown root:root "$GPG_PASSPHRASE_FILE"
    log "ACTION REQUIRED: copy ${GPG_PASSPHRASE_FILE} into your password manager now."
    log "It is the only way to read these backups; losing it loses the history."
  fi

  if grep -q 'REPLACE_WITH_PUSH_TOKEN' "$BACKUP_ENV_FILE"; then
    log "NOTE: ${BACKUP_ENV_FILE} still has the HEARTBEAT_URL placeholder —"
    log "      a failed backup will alert nobody until a push monitor URL is set."
  fi
}

# The daily job: scripts to a stable path, units installed, timer enabled.
install_backup_job() {
  log "installing backup scripts and the daily timer"
  install -m 0755 "${SCRIPT_DIR}/backup.sh" \
                  "${SCRIPT_DIR}/restore.sh" \
                  "${SCRIPT_DIR}/restore-drill.sh" /usr/local/lib/jiuyue/
  install -m 0644 "${SCRIPT_DIR}/systemd/jiuyue-backup.service" \
                  /etc/systemd/system/jiuyue-backup.service
  install -m 0644 "${SCRIPT_DIR}/systemd/jiuyue-backup.timer" \
                  /etc/systemd/system/jiuyue-backup.timer
  systemctl daemon-reload
  systemctl enable --now jiuyue-backup.timer
  log "daily backup scheduled (OnCalendar=18:30 UTC = 02:30 Hong Kong)"
  log "run one now, after filling in BACKUP_REMOTE and HEARTBEAT_URL in ${BACKUP_ENV_FILE}:"
  log "  sudo systemctl start jiuyue-backup.service"
}

# Bound every log producer so `/` stays flat over the long run (docs/monitoring.md §4).
# journald is the shared sink for the backend and Caddy; PostgreSQL writes its
# own file, which journald does not capture on Debian.
ensure_journald_limits() {
  log "installing journald limits (SystemMaxUse=200M)"
  install -d -m 0755 /etc/systemd/journald.conf.d
  install -m 0644 "${SCRIPT_DIR}/systemd/journald.conf.d/10-jiuyue-limits.conf" \
                  /etc/systemd/journald.conf.d/10-jiuyue-limits.conf
  # Storage=persistent needs the directory before journald will write there.
  install -d -m 2755 -o root -g systemd-journal /var/log/journal
  systemctl restart systemd-journald
}

# Debian writes PostgreSQL logs through `pg_ctl -l`, so PostgreSQL's own
# log_rotation_* do nothing; we own rotation with an explicit byte cap. The
# conf.d drop-in is picked up by the restart in ensure_pg_tuning (called next).
ensure_pg_logging() {
  local conf_dir="/etc/postgresql/${PG_VERSION}/main/conf.d"
  [ -d "$conf_dir" ] || die "PostgreSQL conf.d not found at ${conf_dir}; is postgresql-${PG_VERSION} installed?"
  log "installing PostgreSQL log volume settings and rotation ownership"
  install -m 0644 "${SCRIPT_DIR}/postgresql/jiuyue-logging.conf" \
                  "${conf_dir}/30-jiuyue-logging.conf"

  install -m 0644 "${SCRIPT_DIR}/logrotate/jiuyue-postgresql" \
                  /etc/logrotate.d/jiuyue-postgresql
  # logrotate refuses to process two entries for the same log path, so Debian's
  # count-based config for /var/log/postgresql/*.log is retired in favour of the
  # explicit 50M cap above. Idempotent: only the first run moves it.
  if [ -f /etc/logrotate.d/postgresql-common ]; then
    log "retiring Debian's /etc/logrotate.d/postgresql-common (jiuyue owns PG rotation)"
    mv -f /etc/logrotate.d/postgresql-common \
          /etc/logrotate.d/postgresql-common.jiuyue-disabled
  fi
}

# The once-a-minute monitor: disk/memory thresholds, service liveness and the
# Restart=always crash-loop counter, all pushed to a webhook (docs/monitoring.md).
install_monitor() {
  log "installing the monitor script, config and timer"
  install -d -m 0755 /usr/local/lib/jiuyue
  install -d -m 0750 /var/lib/jiuyue-monitor
  install -m 0755 "${SCRIPT_DIR}/monitor/monitor.sh" \
                  "${SCRIPT_DIR}/monitor/install-beszel.sh" /usr/local/lib/jiuyue/

  if [ -f /etc/jiuyue/monitor.env ]; then
    log "/etc/jiuyue/monitor.env exists; keeping it"
  else
    log "writing /etc/jiuyue/monitor.env from the template"
    install -m 0640 -o root -g root "${SCRIPT_DIR}/env/monitor.env.example" /etc/jiuyue/monitor.env
  fi

  # The Beszel agent reads this; created here so the unit's EnvironmentFile
  # resolves even before the view is installed (deploy/monitor/install-beszel.sh).
  if [ -f /etc/jiuyue/beszel.env ]; then
    log "/etc/jiuyue/beszel.env exists; keeping it"
  else
    install -m 0640 -o root -g root "${SCRIPT_DIR}/env/beszel.env.example" /etc/jiuyue/beszel.env
  fi

  install -m 0644 "${SCRIPT_DIR}/systemd/jiuyue-monitor.service" \
                  /etc/systemd/system/jiuyue-monitor.service
  install -m 0644 "${SCRIPT_DIR}/systemd/jiuyue-monitor.timer" \
                  /etc/systemd/system/jiuyue-monitor.timer
  systemctl daemon-reload
  systemctl enable --now jiuyue-monitor.timer

  if grep -q 'REPLACE_WITH_WEBHOOK_TOKEN' /etc/jiuyue/monitor.env; then
    log "NOTE: /etc/jiuyue/monitor.env still has the ALERT_WEBHOOK_URL placeholder —"
    log "      a triggered alert is logged but reaches nobody until a URL is set."
  fi
  log "resource view (Beszel) is installed separately:"
  log "  sudo bash ${SCRIPT_DIR}/monitor/install-beszel.sh --version <pinned>"
}

install_caddyfile() {
  install -d -m 0755 /etc/caddy
  install -m 0644 "${SCRIPT_DIR}/Caddyfile" /etc/caddy/Caddyfile
  # Validate with the real deployment variables before touching the running edge.
  set -a
  # shellcheck disable=SC1090
  . "$CADDY_ENV_FILE"
  set +a
  caddy validate --config /etc/caddy/Caddyfile \
    || die "Caddyfile failed validation; refusing to reload Caddy"
}

enable_services() {
  log "enabling services"
  systemctl enable postgresql
  systemctl enable caddy
  systemctl enable jiuyue
  systemctl restart caddy
  log "Caddy will now attempt to obtain a certificate for ${JIUYUE_DOMAIN}"
  log "DNS A/AAAA must already resolve here and ports 80/443 must be reachable"
}

maybe_deploy() {
  if [ -z "$ARTIFACT" ]; then
    log "provisioning complete. Next: sudo ${SCRIPT_DIR}/deploy.sh --artifact <path-or-url>"
    return 0
  fi
  log "chaining into deploy.sh"
  if [ -n "$SHA256" ]; then
    bash "${SCRIPT_DIR}/deploy.sh" --artifact "$ARTIFACT" --sha256 "$SHA256"
  else
    bash "${SCRIPT_DIR}/deploy.sh" --artifact "$ARTIFACT"
  fi
}

main() {
  parse_args "$@"
  require_repo_assets
  add_caddy_repo
  install_packages
  ensure_user_and_dirs
  ensure_swap
  ensure_journald_limits
  ensure_pg_logging
  ensure_pg_tuning
  write_env_file
  ensure_database
  install_systemd_units
  ensure_backup_config
  install_backup_job
  install_monitor
  write_caddy_env
  install_caddyfile
  enable_services
  maybe_deploy
}

main "$@"
