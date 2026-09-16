#!/usr/bin/env bash
#
# test-deploy-failure-paths.sh — assert deploy.sh's failure paths against a real
# service, without touching /opt and without systemd.
#
# The Release workflow already smoke-tests the *success* path (fake systemctl +
# real binary + real PostgreSQL). This script is about the paths that only
# matter once something has already gone wrong — an untested rollback is a
# rollback that does not work:
#
#   1. rollback          a release whose health check never passes must leave
#                        `current` pointing at the previous release, with the
#                        service restarted onto it (new pid, same version).
#   2. missing checksum  no --sha256 and no readable .sha256 sidecar must exit
#                        non-zero and leave `current` untouched.
#   3. bad checksum      a well-formed sidecar with the wrong digest must exit
#                        non-zero and not activate the release.
#   4. retention         after more than KEEP_RELEASES deploys the oldest
#                        releases are pruned and the newest ones kept.
#   5. idempotency       the same deploy run twice succeeds and does not create
#                        a duplicate release.
#
# Every path deploy.sh knows about is redirected into a sandbox through the
# environment variables it documents (APP_ROOT, RELEASES_DIR, CURRENT_LINK,
# DATA_DIR, ENV_FILE, LOCK_FILE, HEALTH_URL, HEALTH_TIMEOUT), so nothing outside
# --work-dir is written. systemd is replaced by a shim that starts
# `${APP_ROOT}/current/bin/jiuyue-server` and — like real systemd — stops the
# previous process first. Without that, a broken release keeps looking healthy
# behind the still-running old process and the rollback proves nothing.
#
# No containers anywhere (ADR-0010). Nothing here needs network access.
#
# Usage (must run as root — deploy.sh itself refuses to run otherwise):
#
#   sudo env DATABASE_URL=postgres://... JWT_SECRET=... \
#     bash deploy/test-deploy-failure-paths.sh \
#       --artifact ./jiuyue-<version>-x86_64-unknown-linux-gnu.tar.gz \
#       --work-dir /tmp/jiuyue-deploy-failure-paths \
#       [--expected-version <version served by /health>] [--keep 2]
#
# DATABASE_URL / JWT_SECRET are written into each sandbox's env file, exactly
# like the systemd unit's EnvironmentFile is on a real box.
#
# Exit status: 0 only if all five scenarios ran AND every assertion held.
# BASE_PORT (8080), BASE_PORT_RETENTION (8082) and HEALTH_TIMEOUT (30) can be
# overridden from the environment to avoid colliding with a real service.

set -Eeuo pipefail
IFS=$'\n\t'

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
DEPLOY_SH="${SCRIPT_DIR}/deploy.sh"

ARTIFACT=""
WORK_DIR=""
EXPECTED_VERSION=""
KEEP_KEEP=2
BASE_PORT="${BASE_PORT:-8080}"
BASE_PORT_RETENTION="${BASE_PORT_RETENTION:-8082}"
HEALTH_TIMEOUT="${HEALTH_TIMEOUT:-30}"

# Set to 1 by each scenario as its last statement. If a scenario is commented
# out, short-circuited or otherwise skipped, its flag stays 0 and
# assert_every_scenario_ran fails the run — a silent skip must not pass.
ran_rollback=0
ran_no_checksum=0
ran_bad_checksum=0
ran_retention=0
ran_idempotent=0

SANDBOXES=()

log() { printf '%s [failure-paths] %s\n' "$(date -u +%FT%TZ)" "$*" >&2; }
ok()  { printf '%s [failure-paths]   ok: %s\n' "$(date -u +%FT%TZ)" "$*" >&2; }
section() { printf '\n%s [failure-paths] ===== %s =====\n' "$(date -u +%FT%TZ)" "$*" >&2; }
fail() { printf '%s [failure-paths] ASSERTION FAILED: %s\n' "$(date -u +%FT%TZ)" "$*" >&2; exit 1; }

usage() {
  cat >&2 <<'USAGE'
test-deploy-failure-paths.sh — assert deploy.sh's failure paths against a real
service: rollback, missing checksum, bad checksum, retention, idempotency.

Usage:
  sudo env DATABASE_URL=postgres://... JWT_SECRET=... \
    bash deploy/test-deploy-failure-paths.sh \
      --artifact ./jiuyue-<version>-x86_64-unknown-linux-gnu.tar.gz \
      --work-dir /tmp/jiuyue-deploy-failure-paths \
      [--expected-version <version served by /health>] [--keep 2]

Everything deploy.sh writes is redirected inside --work-dir; /opt is never
touched. Exits non-zero if any scenario did not run or any assertion failed.
USAGE
  exit "${1:-0}"
}

cleanup() {
  local sandbox pid
  for sandbox in ${SANDBOXES[@]+"${SANDBOXES[@]}"}; do
    if [ -s "${sandbox}/server.pid" ]; then
      pid="$(cat "${sandbox}/server.pid")"
      kill "$pid" 2>/dev/null || true
    fi
  done
}
trap cleanup EXIT

need_root() {
  if [ "$(id -u)" -ne 0 ]; then
    fail "must run as root (try: sudo env DATABASE_URL=... JWT_SECRET=... bash $0 ...)"
  fi
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --artifact)         ARTIFACT="${2:-}"; shift 2 ;;
      --work-dir)         WORK_DIR="${2:-}"; shift 2 ;;
      --expected-version) EXPECTED_VERSION="${2:-}"; shift 2 ;;
      --keep)             KEEP_KEEP="${2:-}"; shift 2 ;;
      -h|--help)          usage 0 ;;
      *)                  fail "unknown argument: $1 (see --help)" ;;
    esac
  done
  [ -n "$ARTIFACT" ] || fail "--artifact is required (see --help)"
  [ -f "$ARTIFACT" ] || fail "artifact not found: ${ARTIFACT}"
  [ -n "$WORK_DIR" ] || fail "--work-dir is required (see --help)"
  [ -f "$DEPLOY_SH" ] || fail "deploy.sh not found next to this script: ${DEPLOY_SH}"
  [[ "$KEEP_KEEP" =~ ^[0-9]+$ ]] || fail "--keep must be an integer"
}

# --- sandboxes --------------------------------------------------------------

write_env_file() {
  local sandbox="$1" port="$2"
  {
    printf 'DATABASE_URL=%s\n' "$DATABASE_URL"
    printf 'JWT_SECRET=%s\n' "$JWT_SECRET"
    printf 'PORT=%s\n' "$port"
    printf 'RUST_LOG=info\n'
  } > "${sandbox}/etc/jiuyue.env"
  chmod 0644 "${sandbox}/etc/jiuyue.env"
}

install_fake_systemctl() {
  local sandbox="$1"
  cat > "${sandbox}/fakebin/systemctl" <<'SHIM'
#!/usr/bin/env bash
set -Eeuo pipefail
if [ "${1:-}" = restart ] && [ "${2:-}" = jiuyue ]; then
  # Real systemd stops the old process before starting the new one. Without
  # this, the old binary keeps holding the port and a broken release would
  # still look healthy — the rollback scenario would prove nothing.
  if [ -s "${SMOKE_PID}" ]; then
    old="$(cat "${SMOKE_PID}")"
    if kill -0 "$old" 2>/dev/null; then
      kill "$old" 2>/dev/null || true
      for i in $(seq 1 50); do
        kill -0 "$old" 2>/dev/null || break
        sleep 0.1
      done
    fi
  fi
  set -a
  # shellcheck disable=SC1090
  . "${ENV_FILE}"
  set +a
  nohup "${APP_ROOT}/current/bin/jiuyue-server" >>"${SMOKE_LOG}" 2>&1 &
  echo $! >"${SMOKE_PID}"
  # Let the kernel release the listening socket before the health check starts.
  sleep 1
fi
exit 0
SHIM
  chmod 0755 "${sandbox}/fakebin/systemctl"
}

create_sandbox() {
  local sandbox="$1" port="$2"
  mkdir -p "${sandbox}/app" "${sandbox}/data" "${sandbox}/etc" \
           "${sandbox}/run" "${sandbox}/fakebin"
  write_env_file "$sandbox" "$port"
  install_fake_systemctl "$sandbox"
  SANDBOXES+=("$sandbox")
}

# --- running deploy.sh inside a sandbox -------------------------------------

run_deploy() {
  local sandbox="$1" port="$2"
  shift 2
  env \
    "PATH=${sandbox}/fakebin:${PATH}" \
    APP_ROOT="${sandbox}/app" \
    RELEASES_DIR="${sandbox}/app/releases" \
    CURRENT_LINK="${sandbox}/app/current" \
    DATA_DIR="${sandbox}/data" \
    ENV_FILE="${sandbox}/etc/jiuyue.env" \
    SERVICE=jiuyue \
    LOCK_FILE="${sandbox}/run/deploy.lock" \
    HEALTH_URL="http://127.0.0.1:${port}/health" \
    HEALTH_TIMEOUT="${HEALTH_TIMEOUT}" \
    SMOKE_LOG="${sandbox}/server.log" \
    SMOKE_PID="${sandbox}/server.pid" \
    bash "$DEPLOY_SH" "$@"
}

current_target() { readlink -f -- "$1/app/current"; }
health_body()    { curl -fsS --max-time 3 "http://127.0.0.1:$1/health"; }
release_dir() {
  # An existing release is reported in the same canonical form `current` is
  # read back in (which matters when the work dir itself sits behind a symlink);
  # a release that does not exist is just the path we expect to be missing.
  local path
  path="$(printf '%s/app/releases/%s' "$1" "$2")"
  if [ -e "$path" ]; then readlink -f -- "$path"; else printf '%s' "$path"; fi
}

deploy_must_fail() {
  # Runs deploy.sh, keeps the log, returns the exit code without tripping set -e.
  local sandbox="$1" port="$2" log="$3"
  shift 3
  local code
  set +e
  run_deploy "$sandbox" "$port" "$@" >"$log" 2>&1
  code=$?
  set -e
  cat "$log" >&2
  printf '%s' "$code"
}

deploy_must_pass() {
  local sandbox="$1" port="$2" log="$3"
  shift 3
  run_deploy "$sandbox" "$port" "$@" >"$log" 2>&1 \
    || { cat "$log" >&2; fail "deploy.sh failed but this scenario needs a successful deploy (log: ${log})"; }
  cat "$log" >&2
}

# --- scenarios --------------------------------------------------------------

scenario_rollback() {
  section "1/5 rollback: a release that never becomes healthy must roll back"
  local sandbox="${WORK_DIR}/a"
  local ok_dir log bad_log
  log="${WORK_DIR}/scenario-1-rollback.log"
  bad_log="${WORK_DIR}/scenario-1-rollback-bad.log"
  create_sandbox "$sandbox" "$BASE_PORT"

  # --- first: a good release, deployed for real -----------------------------
  deploy_must_pass "$sandbox" "$BASE_PORT" "$log" \
    --artifact "$ARTIFACT" --version rt-ok
  ok_dir="$(release_dir "$sandbox" rt-ok)"
  [ -L "${sandbox}/app/current" ] || fail "current is not a symlink after a successful deploy"
  [ "$(current_target "$sandbox")" = "$ok_dir" ] \
    || fail "current -> $(current_target "$sandbox"), expected ${ok_dir}"
  ok "the good release is active (current -> ${ok_dir})"

  local pid_before body served_version
  pid_before="$(cat "${sandbox}/server.pid")"
  body="$(health_body "$BASE_PORT")" || fail "the good release never answered /health"
  printf '%s' "$body" | grep -q '"status":"ok"' || fail "/health did not report status ok: ${body}"
  served_version="$(printf '%s' "$body" | sed -n 's/.*"version":"\([^"]*\)".*/\1/p')"
  [ -n "$served_version" ] || fail "could not read the version out of /health: ${body}"
  if [ -n "$EXPECTED_VERSION" ] && [ "$served_version" != "$EXPECTED_VERSION" ]; then
    fail "/health serves version ${served_version}, expected ${EXPECTED_VERSION}"
  fi
  ok "the box is serving version ${served_version} (pid ${pid_before})"

  # --- second: a deliberately broken release --------------------------------
  local broken="${WORK_DIR}/broken"
  mkdir -p "${broken}/root"
  tar -xzf "$ARTIFACT" -C "${broken}/root"
  printf '#!/bin/sh\nexit 1\n' > "${broken}/root/bin/jiuyue-server"
  chmod 0755 "${broken}/root/bin/jiuyue-server"
  ( cd "${broken}/root" && tar -czf ../broken.tar.gz bin dist VERSION )
  ( cd "${broken}" && sha256sum broken.tar.gz > broken.tar.gz.sha256 )
  grep -q 'exit 1' "${broken}/root/bin/jiuyue-server" \
    || fail "the broken artifact was not built as intended"
  ok "built a broken release artifact whose server exits immediately"

  # --- deploy it: must fail, and current must go back ----------------------
  local code
  code="$(deploy_must_fail "$sandbox" "$BASE_PORT" "$bad_log" \
    --artifact "${broken}/broken.tar.gz" --version rt-bad)"

  [ "$code" -ne 0 ] || fail "deploy.sh exited 0 for a release that never became healthy"
  ok "the broken release was refused (exit ${code})"

  [ -d "$(release_dir "$sandbox" rt-bad)" ] \
    || fail "the broken release was never installed — the scenario never reached the rollback"
  ok "the broken release really was installed before failing (rt-bad exists)"

  grep -q 'rolling back to' "$bad_log" || fail "deploy.sh never logged the rollback"
  grep -q 'rollback healthy' "$bad_log" \
    || fail "deploy.sh did not report a healthy rollback"

  [ "$(current_target "$sandbox")" = "$ok_dir" ] \
    || fail "after a failed health check current -> $(current_target "$sandbox"), expected the previous release ${ok_dir}"
  ok "current points back at the previous release (${ok_dir})"

  # Repointing the symlink is not enough: the service must be running the
  # previous release, i.e. a *new* process serving the *old* version.
  local pid_after
  pid_after="$(cat "${sandbox}/server.pid")"
  [ "$pid_after" != "$pid_before" ] \
    || fail "the service was not restarted onto the previous release (pid unchanged: ${pid_after})"
  kill -0 "$pid_after" 2>/dev/null || fail "the restarted process ${pid_after} is not alive"
  body="$(health_body "$BASE_PORT")" || fail "the box is not healthy after the rollback"
  printf '%s' "$body" | grep -q '"status":"ok"' || fail "/health is not ok after the rollback: ${body}"
  printf '%s' "$body" | grep -q "\"version\":\"${served_version}\"" \
    || fail "after the rollback /health serves ${body}, expected version ${served_version}"
  ok "the service was restarted onto it (pid ${pid_before} -> ${pid_after}, version ${served_version})"

  ran_rollback=1
  ok "scenario 1 passed"
}

scenario_no_checksum() {
  section "2/5 refuse: no --sha256 and no readable .sha256 sidecar"
  local sandbox="${WORK_DIR}/a"
  local dir="${WORK_DIR}/no-sidecar"
  local log="${WORK_DIR}/scenario-2-no-checksum.log"
  mkdir -p "$dir"
  cp -f "$ARTIFACT" "${dir}/artifact.tar.gz"
  rm -f "${dir}/artifact.tar.gz.sha256"
  [ ! -e "${dir}/artifact.tar.gz.sha256" ] || fail "the sidecar-less artifact still has a sidecar"

  local before after code
  [ -L "${sandbox}/app/current" ] || fail "no active release to protect — scenario 1 must run first"
  before="$(current_target "$sandbox")"

  code="$(deploy_must_fail "$sandbox" "$BASE_PORT" "$log" \
    --artifact "${dir}/artifact.tar.gz" --version rt-unverified)"

  [ "$code" -ne 0 ] || fail "deploy.sh exited 0 with no checksum at all — it must refuse"
  ok "deploy.sh refused the unverified artifact (exit ${code})"
  grep -q 'refusing to deploy an unverified artifact' "$log" \
    || fail "deploy.sh did not say why it refused the unverified artifact"
  ok "the refusal is explicit in the log"
  [ ! -e "$(release_dir "$sandbox" rt-unverified)" ] \
    || fail "the unverified release was installed anyway"
  after="$(current_target "$sandbox")"
  [ "$after" = "$before" ] || fail "current moved from ${before} to ${after} despite the refusal"
  ok "current is untouched (${after})"

  ran_no_checksum=1
  ok "scenario 2 passed"
}

scenario_bad_checksum() {
  section "3/5 refuse: a well-formed sidecar with the wrong digest"
  local sandbox="${WORK_DIR}/a"
  local dir="${WORK_DIR}/bad-sidecar"
  local log="${WORK_DIR}/scenario-3-bad-checksum.log"
  mkdir -p "$dir"
  cp -f "$ARTIFACT" "${dir}/artifact.tar.gz"
  # 64 hex characters — well-formed, just not the digest of this file.
  printf '%064d  artifact.tar.gz\n' 0 > "${dir}/artifact.tar.gz.sha256"

  local before after code
  [ -L "${sandbox}/app/current" ] || fail "no active release to protect — scenario 1 must run first"
  before="$(current_target "$sandbox")"

  code="$(deploy_must_fail "$sandbox" "$BASE_PORT" "$log" \
    --artifact "${dir}/artifact.tar.gz" --version rt-badsum)"

  [ "$code" -ne 0 ] || fail "deploy.sh exited 0 for an artifact whose digest does not match"
  ok "deploy.sh refused the mismatching artifact (exit ${code})"
  grep -q 'sha256 mismatch' "$log" || fail "deploy.sh never reported the checksum mismatch"
  ok "the mismatch is explicit in the log"
  [ ! -e "$(release_dir "$sandbox" rt-badsum)" ] \
    || fail "the mismatching release was installed anyway"
  after="$(current_target "$sandbox")"
  [ "$after" = "$before" ] || fail "current moved from ${before} to ${after} despite the refusal"
  ok "current is untouched (${after})"

  ran_bad_checksum=1
  ok "scenario 3 passed"
}

scenario_retention() {
  section "4/5 retention: more than KEEP_RELEASES deploys prune the oldest"
  local sandbox="${WORK_DIR}/b"
  local log="${WORK_DIR}/scenario-4-retention.log"
  local i dirs count gone kept expected_keep
  create_sandbox "$sandbox" "$BASE_PORT_RETENTION"

  for i in 1 2 3 4 5; do
    log "deploying rt-keep-${i} (--keep ${KEEP_KEEP})"
    deploy_must_pass "$sandbox" "$BASE_PORT_RETENTION" "$log" \
      --artifact "$ARTIFACT" --version "rt-keep-${i}" --keep "$KEEP_KEEP"
    # Distinct directory mtimes keep prune_releases' oldest-first ordering
    # deterministic even on a fast filesystem.
    sleep 1
  done

  # KEEP_RELEASES prunes *stale* releases; the active one is never counted, so
  # five deploys with --keep 2 must leave 2 + 1 directories.
  expected_keep=$(( KEEP_KEEP + 1 ))
  dirs="$(find "${sandbox}/app/releases" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | sort)"
  count="$(printf '%s\n' "$dirs" | grep -c . || true)"
  log "releases kept: $(printf '%s' "$dirs" | tr '\n' ' ')(${count})"
  [ "$count" -eq "$expected_keep" ] \
    || fail "kept ${count} releases, expected ${expected_keep} (--keep ${KEEP_KEEP} plus the active one)"

  for gone in rt-keep-1 rt-keep-2; do
    [ ! -e "$(release_dir "$sandbox" "$gone")" ] || fail "the oldest release ${gone} was not pruned"
  done
  ok "the oldest releases were pruned (rt-keep-1, rt-keep-2)"
  for kept in rt-keep-3 rt-keep-4 rt-keep-5; do
    [ -d "$(release_dir "$sandbox" "$kept")" ] || fail "the newest release ${kept} was pruned"
  done
  ok "the newest releases were kept (rt-keep-3, rt-keep-4, rt-keep-5)"

  [ "$(current_target "$sandbox")" = "$(release_dir "$sandbox" rt-keep-5)" ] \
    || fail "current -> $(current_target "$sandbox") after pruning; the active release must never be deleted"
  ok "current still points at the newest release"
  grep -q 'pruning old release' "$log" || fail "deploy.sh never logged a prune"
  ok "the prune really happened (log evidence)"
  health_body "$BASE_PORT_RETENTION" | grep -q '"status":"ok"' \
    || fail "the box is not healthy after pruning"
  ok "the box is healthy after pruning"

  ran_retention=1
  ok "scenario 4 passed"
}

scenario_idempotent() {
  section "5/5 idempotency: running the same deploy twice"
  local sandbox="${WORK_DIR}/a"
  local log="${WORK_DIR}/scenario-5-idempotent.log"
  local first second count

  deploy_must_pass "$sandbox" "$BASE_PORT" "$log" \
    --artifact "$ARTIFACT" --version rt-idem
  first="$(current_target "$sandbox")"
  [ "$first" = "$(release_dir "$sandbox" rt-idem)" ] \
    || fail "after the first deploy current -> ${first}"
  ok "the first rt-idem deploy succeeded (current -> ${first})"

  deploy_must_pass "$sandbox" "$BASE_PORT" "$log" \
    --artifact "$ARTIFACT" --version rt-idem
  second="$(current_target "$sandbox")"
  [ "$second" = "$first" ] || fail "the second deploy moved current from ${first} to ${second}"
  ok "re-running the same deploy succeeded and left current where it was"

  count="$(find "${sandbox}/app/releases" -mindepth 1 -maxdepth 1 -type d -name 'rt-idem' | wc -l)"
  count="${count// /}"
  [ "$count" -eq 1 ] || fail "the same version produced ${count} release directories"
  ok "exactly one rt-idem release directory exists"
  grep -q 'replacing existing release directory' "$log" \
    || fail "the second deploy did not replace the existing release directory"
  ok "the second run replaced the existing release instead of duplicating it"
  health_body "$BASE_PORT" | grep -q '"status":"ok"' \
    || fail "the box is not healthy after the repeated deploy"
  ok "the box is healthy after the repeated deploy"

  ran_idempotent=1
  ok "scenario 5 passed"
}

# --- anti silent-skip -------------------------------------------------------

assert_every_scenario_ran() {
  section "anti silent-skip: all five scenarios really ran"
  local missing=0 f
  if [ "$ran_rollback" -ne 1 ]; then printf '::error::scenario 1 (rollback) did not run\n'; missing=1; fi
  if [ "$ran_no_checksum" -ne 1 ]; then printf '::error::scenario 2 (missing checksum) did not run\n'; missing=1; fi
  if [ "$ran_bad_checksum" -ne 1 ]; then printf '::error::scenario 3 (bad checksum) did not run\n'; missing=1; fi
  if [ "$ran_retention" -ne 1 ]; then printf '::error::scenario 4 (retention) did not run\n'; missing=1; fi
  if [ "$ran_idempotent" -ne 1 ]; then printf '::error::scenario 5 (idempotency) did not run\n'; missing=1; fi
  [ "$missing" -eq 0 ] || fail "one or more failure-path scenarios never executed"

  for f in scenario-1-rollback.log \
           scenario-1-rollback-bad.log \
           scenario-2-no-checksum.log \
           scenario-3-bad-checksum.log \
           scenario-4-retention.log \
           scenario-5-idempotent.log; do
    [ -s "${WORK_DIR}/${f}" ] || fail "missing evidence log ${f}"
  done
  ok "5/5 scenarios ran, 6/6 evidence logs present"
}

main() {
  parse_args "$@"
  need_root
  need_cmd curl
  need_cmd sha256sum
  need_cmd tar
  need_cmd flock
  need_cmd find
  need_cmd sed
  need_cmd awk
  # deploy.sh installs releases owned by root:jiuyue, so the group must exist
  # exactly as it does on a provisioned box.
  groupadd -f jiuyue

  mkdir -p "$WORK_DIR"
  # readlink -f canonicalises the whole path, so compare like with like.
  WORK_DIR="$(readlink -f -- "$WORK_DIR")"
  log "work dir   : ${WORK_DIR}"
  log "artifact   : ${ARTIFACT}"
  log "artifact sha256: $(sha256sum "$ARTIFACT" | awk '{print $1}')"
  log "deploy.sh  : ${DEPLOY_SH}"

  scenario_rollback
  scenario_no_checksum
  scenario_bad_checksum
  scenario_retention
  scenario_idempotent

  assert_every_scenario_ran
  section "all five failure paths asserted"
}

main "$@"
