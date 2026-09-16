#!/usr/bin/env bash
#
# monitor.sh — the container-free host/service monitor for the 2 GB box.
#
# One small script, run by `jiuyue-monitor.timer` every minute. It does NOT try
# to be Prometheus: it answers four questions and pushes the answer to a webhook
# a human actually reads. `Restart=always` on the backend hides crashes by
# design, so a check like this is the only thing that makes a crash loop visible.
#
# Checks (each is independent; `all` runs them in order):
#   resources  disk usage of DISK_PATHS and MemAvailable, against two thresholds
#   services   `systemctl is-active` for MONITOR_SERVICES, after N strikes
#   restarts   automatic-restart count in a sliding window for RESTART_SERVICE
#   all        everything above
#
# Subcommands:
#   resources | services | restarts | all     run checks (all is the timer default)
#   test-alert                                send one synthetic alert, for wiring tests
#   status                                    print the last status.json
#
# Alerting is stateful and deduplicated:
#   * each alert key remembers its last severity; a repeat only re-sends after
#     ALERT_COOLDOWN_SEC, so a disk that stays full does not page every minute;
#   * when a condition clears, a `recovery` notice is sent once
#     (unless ALERT_NOTIFY_RECOVERY=0).
#
# A crash loop vs a normal deploy:
#   * `systemctl show -p NRestarts` counts AUTOMATIC (`Restart=`) restarts; an
#     explicit `systemctl restart` (what deploy.sh does) resets it to 0.
#   * systemd logs "Scheduled restart job" for every automatic restart. The
#     restart check counts those inside RESTART_WINDOW_SEC and only alerts past
#     RESTART_THRESHOLD, so a single deploy is silent while five crashes in ten
#     minutes is not.
#   * every observed change in NRestarts is appended to restarts.log with a
#     reason, so the count and the reason are recorded, not just repeated.
#
# Everything is environment-overridable (see deploy/env/monitor.env.example) so
# the whole thing can be driven against a temporary state directory in CI. The
# systemctl/journalctl/df binaries are overridable too, which is how the
# alert-routing test exercises a crash loop without a real crash.
#
# No containers anywhere (ADR-0010).

set -Eeuo pipefail
IFS=$'\n\t'

# --- configuration (the systemd unit loads /etc/jiuyue/monitor.env) ---------
ALERT_WEBHOOK_URL="${ALERT_WEBHOOK_URL:-}"
ALERT_COOLDOWN_SEC="${ALERT_COOLDOWN_SEC:-3600}"
ALERT_STATE_DIR="${ALERT_STATE_DIR:-/var/lib/jiuyue-monitor}"
ALERT_HOSTNAME="${ALERT_HOSTNAME:-$(hostname 2>/dev/null || echo unknown)}"
ALERT_NOTIFY_RECOVERY="${ALERT_NOTIFY_RECOVERY:-1}"

MONITOR_SERVICES="${MONITOR_SERVICES:-jiuyue caddy postgresql}"
DISK_PATHS="${DISK_PATHS:-/}"
DISK_WARN_PERCENT="${DISK_WARN_PERCENT:-80}"
DISK_CRIT_PERCENT="${DISK_CRIT_PERCENT:-90}"
MEM_AVAILABLE_WARN_PERCENT="${MEM_AVAILABLE_WARN_PERCENT:-15}"
MEM_AVAILABLE_CRIT_PERCENT="${MEM_AVAILABLE_CRIT_PERCENT:-8}"
SERVICE_DOWN_STRIKES="${SERVICE_DOWN_STRIKES:-2}"
RESTART_SERVICE="${RESTART_SERVICE:-jiuyue}"
RESTART_WINDOW_SEC="${RESTART_WINDOW_SEC:-600}"
RESTART_THRESHOLD="${RESTART_THRESHOLD:-5}"

SYSTEMCTL="${SYSTEMCTL:-systemctl}"
JOURNALCTL="${JOURNALCTL:-journalctl}"
DF="${DF:-df}"
MEMINFO="${MEMINFO:-/proc/meminfo}"
CURL="${CURL:-curl}"

STATUS_JSON_PATH="${ALERT_STATE_DIR}/status.json"
RESTARTS_LOG="${ALERT_STATE_DIR}/restarts.log"

DELIVERY_FAILED=0
declare -A METRICS=()

# --- helpers ----------------------------------------------------------------
log() { printf '%s [monitor] %s\n' "$(date -u +%FT%TZ)" "$*" >&2; }
die() { printf '%s [monitor] ERROR: %s\n' "$(date -u +%FT%TZ)" "$*" >&2; exit 1; }

usage() {
  sed -n '3,30p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit "${1:-0}"
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

# Reduce a free-form key to something safe for a filename.
key_of() {
  printf '%s' "$1" | tr -c 'A-Za-z0-9._-' '_'
}

json_escape() {
  local s="$1"
  s="${s//\\/\\\\}"
  s="${s//\"/\\\"}"
  s="${s//$'\n'/ }"
  s="${s//$'\r'/}"
  s="${s//$'\t'/ }"
  printf '%s' "$s"
}

# Emit a JSON scalar: a bare number when the value is numeric, otherwise null.
json_scalar() {
  local v="$1"
  if [[ "$v" =~ ^-?[0-9]+([.][0-9]+)?$ ]]; then
    printf '%s' "$v"
  else
    printf 'null'
  fi
}

state_get() {
  local file="${ALERT_STATE_DIR}/$1" fallback="${2:-}"
  if [ -f "$file" ]; then
    cat -- "$file"
  else
    printf '%s' "$fallback"
  fi
}

state_set() {
  printf '%s' "$2" > "${ALERT_STATE_DIR}/$1"
}

record_metric() {
  METRICS["$1"]="$2"
}

# --- alerting ---------------------------------------------------------------
# POST a JSON document to ALERT_WEBHOOK_URL. The payload carries both
# machine-readable fields and a pre-rendered `text` line, so the same message is
# usable by a generic webhook relay, ntfy, Slack or a chat bot without a custom
# adapter. Delivery is best-effort; the exit code of a FAILED delivery is
# surfaced through DELIVERY_FAILED so the unit shows up in `systemctl --failed`.
send_alert() {
  local event="$1" severity="$2" target="$3" value="$4" threshold="$5" message="$6"
  local ts body text
  ts="$(date -u +%FT%TZ)"
  text="[${severity}] ${event} ${target}: ${message}"
  body="$(printf '{"source":"jiuyue-monitor","host":"%s","event":"%s","severity":"%s","target":"%s","value":%s,"threshold":%s,"message":"%s","text":"%s","timestamp":"%s"}' \
    "$(json_escape "$ALERT_HOSTNAME")" \
    "$(json_escape "$event")" \
    "$(json_escape "$severity")" \
    "$(json_escape "$target")" \
    "$(json_scalar "$value")" \
    "$(json_scalar "$threshold")" \
    "$(json_escape "$message")" \
    "$(json_escape "$text")" \
    "$(json_escape "$ts")")"

  if [ -z "$ALERT_WEBHOOK_URL" ]; then
    log "ERROR: ${text} — no ALERT_WEBHOOK_URL set, so nobody will see this"
    DELIVERY_FAILED=1
    return 0
  fi
  if "$CURL" -fsS --max-time 10 -X POST \
       -H 'Content-Type: application/json' --data "$body" "$ALERT_WEBHOOK_URL" >/dev/null; then
    log "alert sent: ${text}"
  else
    log "ERROR: webhook delivery failed for ${text}"
    DELIVERY_FAILED=1
  fi
  return 0
}

# emit_alert <key> <event> <severity> <target> <value> <threshold> <message>
# severity `ok` clears a previously-firing key and (optionally) announces it.
emit_alert() {
  local key event severity target value threshold message
  key="$(key_of "$1")"; event="$2"; severity="$3"; target="$4"
  value="$5"; threshold="$6"; message="$7"
  local now prev last
  now="$(date +%s)"
  prev="$(state_get "${key}.state" ok)"
  last="$(state_get "${key}.ts" 0)"
  [[ "$last" =~ ^[0-9]+$ ]] || last=0

  case "$severity" in
    ok)
      if [ "$prev" != ok ]; then
        state_set "${key}.state" ok
        if [ "$ALERT_NOTIFY_RECOVERY" = 1 ]; then
          send_alert "$event" recovery "$target" "$value" "$threshold" "recovered: ${message}"
        fi
      fi
      ;;
    *)
      if [ "$prev" != "$severity" ] || [ $(( now - last )) -ge "$ALERT_COOLDOWN_SEC" ]; then
        send_alert "$event" "$severity" "$target" "$value" "$threshold" "$message"
        state_set "${key}.ts" "$now"
      else
        log "suppressed (cooldown) [${severity}] ${event} ${target}: ${message}"
      fi
      state_set "${key}.state" "$severity"
      ;;
  esac
}

# --- checks -----------------------------------------------------------------
check_disk() {
  local path pct
  for path in "${DISK_PATHS_ARR[@]}"; do
    # `df -P` is the portable, one-line-per-filesystem format; `-P` also stops
    # long device names from wrapping the columns. Do NOT index a fixed field:
    # a mount path with spaces (common on dev machines) shifts every field, so
    # scan from the end for the field ending in `%` (the Capacity column, which
    # is immediately before the mount point).
    pct="$("$DF" -P "$path" 2>/dev/null \
      | awk 'NR==2{for(i=NF;i>0;i--){if($i ~ /%$/){gsub(/%/,"",$i);print $i;exit}}}')"
    if [ -z "$pct" ]; then
      log "WARNING: could not read disk usage for ${path}"
      continue
    fi
    record_metric "disk_${path}_percent" "$pct"
    if [ "$pct" -ge "$DISK_CRIT_PERCENT" ]; then
      emit_alert "disk_${path}" disk_usage critical "$path" "$pct" "$DISK_CRIT_PERCENT" \
        "${path} is ${pct}% full (critical at ${DISK_CRIT_PERCENT}%)"
    elif [ "$pct" -ge "$DISK_WARN_PERCENT" ]; then
      emit_alert "disk_${path}" disk_usage warning "$path" "$pct" "$DISK_WARN_PERCENT" \
        "${path} is ${pct}% full (warning at ${DISK_WARN_PERCENT}%)"
    else
      emit_alert "disk_${path}" disk_usage ok "$path" "$pct" "$DISK_WARN_PERCENT" \
        "${path} is ${pct}% full"
    fi
  done
}

check_memory() {
  local total avail pct
  total="$(awk '/^MemTotal:/{print $2}' "$MEMINFO" 2>/dev/null || true)"
  avail="$(awk '/^MemAvailable:/{print $2}' "$MEMINFO" 2>/dev/null || true)"
  if [ -z "$total" ] || [ -z "$avail" ] || [ "$total" -eq 0 ]; then
    log "WARNING: could not read MemTotal/MemAvailable from ${MEMINFO}"
    return 0
  fi
  pct=$(( avail * 100 / total ))
  record_metric "mem_available_percent" "$pct"
  if [ "$pct" -le "$MEM_AVAILABLE_CRIT_PERCENT" ]; then
    emit_alert "memory" memory_available critical "MemAvailable" "$pct" "$MEM_AVAILABLE_CRIT_PERCENT" \
      "only ${pct}% of RAM available (critical at <= ${MEM_AVAILABLE_CRIT_PERCENT}%)"
  elif [ "$pct" -le "$MEM_AVAILABLE_WARN_PERCENT" ]; then
    emit_alert "memory" memory_available warning "MemAvailable" "$pct" "$MEM_AVAILABLE_WARN_PERCENT" \
      "only ${pct}% of RAM available (warning at <= ${MEM_AVAILABLE_WARN_PERCENT}%)"
  else
    emit_alert "memory" memory_available ok "MemAvailable" "$pct" "$MEM_AVAILABLE_WARN_PERCENT" \
      "${pct}% of RAM available"
  fi
}

check_services() {
  local svc key strikes detail
  for svc in "${MONITOR_SERVICES_ARR[@]}"; do
    key="svc_${svc}"
    if "$SYSTEMCTL" is-active --quiet "$svc" 2>/dev/null; then
      state_set "${key}.strikes" 0
      emit_alert "$key" service_down ok "$svc" "" "" "${svc} is running"
    else
      strikes=$(( $(state_get "${key}.strikes" 0) + 1 ))
      state_set "${key}.strikes" "$strikes"
      detail="$("$SYSTEMCTL" is-active "$svc" 2>/dev/null || true)"
      if [ "$strikes" -ge "$SERVICE_DOWN_STRIKES" ]; then
        emit_alert "$key" service_down critical "$svc" "" "" \
          "${svc} is not active (${detail:-unknown}); ${strikes} consecutive failed checks"
      else
        log "service ${svc} not active (strike ${strikes}/${SERVICE_DOWN_STRIKES}; not alerting yet)"
      fi
    fi
  done
}

# Append a line whenever NRestarts moves, so the count and the reason are
# recorded even when no alert fires (a single deploy restart is normal).
record_restart() {
  local svc="$1" n="$2" count="$3" reason="$4" kind
  local key="restart_${svc}"
  local last
  last="$(state_get "${key}.n" "")"
  if [ "$n" = "$last" ]; then
    return 0
  fi
  if [ "$count" -gt 0 ]; then
    kind="auto-restart"
  else
    kind="manual-or-deploy"
  fi
  mkdir -p -- "$ALERT_STATE_DIR" 2>/dev/null || true
  printf '%s service=%s kind=%s NRestarts=%s window_count=%s reason=%s\n' \
    "$(date -u +%FT%TZ)" "$svc" "$kind" "$n" "$count" "${reason:-unknown}" \
    >> "$RESTARTS_LOG" 2>/dev/null || true
  state_set "${key}.n" "$n"
  log "recorded restart: ${svc} kind=${kind} NRestarts=${n} window_count=${count} reason=${reason:-unknown}"
}

check_restarts() {
  local svc="$RESTART_SERVICE"
  local events count reason n
  events="$("$JOURNALCTL" -u "$svc" --since "-${RESTART_WINDOW_SEC}s" -o cat 2>/dev/null || true)"
  # systemd emits this exact line for every automatic (Restart=) restart.
  count="$(printf '%s\n' "$events" | grep -c 'Scheduled restart job' || true)"
  [ -n "$count" ] || count=0
  reason="$(printf '%s\n' "$events" | grep -E 'Failed with result|Main process exited' | tail -n 1 || true)"
  n="$("$SYSTEMCTL" show -p NRestarts --value "$svc" 2>/dev/null || true)"
  [ -n "$n" ] || n=0
  record_metric "restart_${svc}_window_count" "$count"
  record_metric "restart_${svc}_nrestarts" "$n"
  record_restart "$svc" "$n" "$count" "$reason"

  if [ "$count" -ge "$RESTART_THRESHOLD" ]; then
    emit_alert "restart_${svc}" service_restart_loop critical "$svc" "$count" "$RESTART_THRESHOLD" \
      "${svc} auto-restarted ${count} times in the last ${RESTART_WINDOW_SEC}s (threshold ${RESTART_THRESHOLD}); last reason: ${reason:-unknown}"
  else
    emit_alert "restart_${svc}" service_restart_loop ok "$svc" "$count" "$RESTART_THRESHOLD" \
      "${svc} auto-restarts in the last ${RESTART_WINDOW_SEC}s: ${count}"
  fi
}

# --- status file (a usable text view even without the dashboard) ------------
write_status() {
  local first=1 k
  mkdir -p -- "$ALERT_STATE_DIR" 2>/dev/null || true
  {
    printf '{"timestamp":"%s","host":"%s","checks":{' \
      "$(date -u +%FT%TZ)" "$(json_escape "$ALERT_HOSTNAME")"
    for k in "${!METRICS[@]}"; do
      if [ "$first" -eq 1 ]; then
        first=0
      else
        printf ','
      fi
      printf '"%s":"%s"' "$(json_escape "$k")" "$(json_escape "${METRICS[$k]}")"
    done
    printf '}}\n'
  } > "$STATUS_JSON_PATH" 2>/dev/null || log "WARNING: could not write ${STATUS_JSON_PATH}"
}

# --- entry ------------------------------------------------------------------
validate_config() {
  local name value
  for name in ALERT_COOLDOWN_SEC DISK_WARN_PERCENT DISK_CRIT_PERCENT \
              MEM_AVAILABLE_WARN_PERCENT MEM_AVAILABLE_CRIT_PERCENT \
              SERVICE_DOWN_STRIKES RESTART_WINDOW_SEC RESTART_THRESHOLD; do
    value="${!name}"
    [[ "$value" =~ ^[0-9]+$ ]] || die "${name} must be a non-negative integer (got '${value}')"
  done
  [ "$DISK_CRIT_PERCENT" -ge "$DISK_WARN_PERCENT" ] || die "DISK_CRIT_PERCENT must be >= DISK_WARN_PERCENT"
  read -r -a DISK_PATHS_ARR <<< "$DISK_PATHS"
  read -r -a MONITOR_SERVICES_ARR <<< "$MONITOR_SERVICES"
  [ "${#DISK_PATHS_ARR[@]}" -gt 0 ] || die "DISK_PATHS is empty"
}

main() {
  local cmd="${1:-all}"
  case "$cmd" in
    -h|--help) usage 0 ;;
  esac
  validate_config
  mkdir -p -- "$ALERT_STATE_DIR" 2>/dev/null || die "cannot create state dir ${ALERT_STATE_DIR}"

  case "$cmd" in
    resources)
      need_cmd "$DF"
      check_disk
      check_memory
      ;;
    services)
      need_cmd "$SYSTEMCTL"
      check_services
      ;;
    restarts)
      need_cmd "$SYSTEMCTL"
      need_cmd "$JOURNALCTL"
      check_restarts
      ;;
    all)
      need_cmd "$DF"
      need_cmd "$SYSTEMCTL"
      need_cmd "$JOURNALCTL"
      check_disk
      check_memory
      check_services
      check_restarts
      ;;
    test-alert)
      need_cmd "$CURL"
      send_alert "test" info "monitor" "" "" "synthetic test alert from jiuyue-monitor on ${ALERT_HOSTNAME}"
      ;;
    status)
      if [ -f "$STATUS_JSON_PATH" ]; then
        cat -- "$STATUS_JSON_PATH"
      else
        die "no status file yet at ${STATUS_JSON_PATH}"
      fi
      return 0
      ;;
    *) die "unknown subcommand: ${cmd} (try --help)" ;;
  esac

  if [ "$cmd" != test-alert ] && [ "$cmd" != status ]; then
    write_status
  fi
  if [ "$DELIVERY_FAILED" -ne 0 ]; then
    die "one or more alerts could not be delivered"
  fi
}

main "$@"
