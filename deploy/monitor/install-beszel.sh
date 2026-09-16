#!/usr/bin/env bash
#
# install-beszel.sh — install the Beszel hub and/or agent natively, no containers.
#
# Beszel is the resource-usage VIEW for this box (see docs/monitoring.md and
# docs/research/2026-09-16-vps-2gb-ops-research.md §(e)): a hub + one agent,
# agent ~10-30 MB (maintainer: <10 MB) and hub ~50-100 MB measured, which fits
# the ~482 MB of headroom on the 2 GB server. It is deliberately NOT
# Prometheus+Grafana (three services before a single alert rule) and NOT Netdata
# (150-400 MB RSS and about 1.2 GB of disk per day — wrong tool for this machine).
#
# No containers (ADR-0010): the release tarballs are unpacked to INSTALL_DIR and
# run by systemd. The hub binds 127.0.0.1 only; reach it through an SSH tunnel.
#
# Checksum discipline matches deploy/deploy.sh: the asset is REFUSED unless a
# matching sha256 is supplied (explicit --sha256-hub/--sha256-agent, or an entry
# in the release checksums file). An unverified binary is never installed.
#
# Usage:
#   sudo deploy/monitor/install-beszel.sh --version 0.12.1
#   sudo deploy/monitor/install-beszel.sh --version 0.12.1 --component agent
#   deploy/monitor/install-beszel.sh --version 0.12.1 --component hub \
#        --asset-url ./fake.tar.gz --checksums-file ./checksums.txt \
#        --install-dir /tmp/opt --no-systemd        # tests: no root, no systemd
#
# Options:
#   --version <v>          release version, no leading `v` (REQUIRED)
#   --component hub|agent|both          (default both)
#   --arch amd64|arm64                  (default amd64)
#   --asset-base-url URL   release download base (default the GitHub release)
#   --asset-url PATH|URL   override the hub asset location (tests / mirrors)
#   --agent-asset-url P|U  override the agent asset location
#   --checksums-file P|URL released checksums.txt (default: fetched per component)
#   --checksums-url URL    override the default checksums location
#   --sha256-hub HEX       explicit hub checksum (skips the checksums file)
#   --sha256-agent HEX     explicit agent checksum
#   --install-dir DIR      where binaries go (default /opt/beszel)
#   --data-dir DIR         hub state (default /var/lib/beszel)
#   --systemd-dir DIR      unit directory (default /etc/systemd/system)
#   --no-systemd           install binaries only (tests / non-systemd hosts)
#   -h | --help

set -Eeuo pipefail
IFS=$'\n\t'

VERSION=""
COMPONENT="both"
ARCH="${BESZEL_ARCH:-amd64}"
ASSET_BASE_URL="${BESZEL_ASSET_BASE_URL:-https://github.com/henrygd/beszel/releases/download}"
HUB_ASSET_URL=""
AGENT_ASSET_URL=""
CHECKSUMS_FILE=""
CHECKSUMS_URL=""
SHA256_HUB=""
SHA256_AGENT=""
INSTALL_DIR="${BESZEL_INSTALL_DIR:-/opt/beszel}"
DATA_DIR="${BESZEL_DATA_DIR:-/var/lib/beszel}"
SYSTEMD_DIR="${BESZEL_SYSTEMD_DIR:-/etc/systemd/system}"
INSTALL_SYSTEMD=1

TMP_DIR=""

# Resolve the unit files relative to this script so the installer works from a
# checkout (`deploy/monitor/install-beszel.sh` -> `deploy/systemd/`) regardless
# of the caller's working directory.
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

log() { printf '%s [beszel] %s\n' "$(date -u +%FT%TZ)" "$*" >&2; }
die() { printf '%s [beszel] ERROR: %s\n' "$(date -u +%FT%TZ)" "$*" >&2; exit 1; }

usage() {
  sed -n '3,45p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit "${1:-0}"
}

cleanup() {
  if [ -n "$TMP_DIR" ] && [ -d "$TMP_DIR" ]; then
    rm -rf -- "$TMP_DIR"
  fi
}
trap cleanup EXIT

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --version)         VERSION="${2:-}"; shift 2 ;;
      --component)       COMPONENT="${2:-}"; shift 2 ;;
      --arch)            ARCH="${2:-}"; shift 2 ;;
      --asset-base-url)  ASSET_BASE_URL="${2:-}"; shift 2 ;;
      --asset-url)       HUB_ASSET_URL="${2:-}"; shift 2 ;;
      --agent-asset-url) AGENT_ASSET_URL="${2:-}"; shift 2 ;;
      --checksums-file)  CHECKSUMS_FILE="${2:-}"; shift 2 ;;
      --checksums-url)   CHECKSUMS_URL="${2:-}"; shift 2 ;;
      --sha256-hub)      SHA256_HUB="${2:-}"; shift 2 ;;
      --sha256-agent)    SHA256_AGENT="${2:-}"; shift 2 ;;
      --install-dir)     INSTALL_DIR="${2:-}"; shift 2 ;;
      --data-dir)        DATA_DIR="${2:-}"; shift 2 ;;
      --systemd-dir)     SYSTEMD_DIR="${2:-}"; shift 2 ;;
      --no-systemd)      INSTALL_SYSTEMD=0; shift ;;
      -h|--help)         usage 0 ;;
      *)                 die "unknown argument: $1 (see --help)" ;;
    esac
  done
  [ -n "$VERSION" ] || die "--version is required (a pinned release, no leading v)"
  case "$VERSION" in v*) die "--version must not carry a leading 'v' (got '${VERSION}')" ;; esac
  case "$COMPONENT" in hub|agent|both) ;; *) die "--component must be hub, agent or both" ;; esac
  case "$ARCH" in amd64|arm64) ;; *) die "--arch must be amd64 or arm64" ;; esac
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

# Fetch text from a URL or a local path.
fetch_text() {
  local source="$1"
  case "$source" in
    http://*|https://*) curl -fLsS --retry 3 --retry-delay 2 "$source" ;;
    file://*)           cat -- "${source#file://}" ;;
    *)                  cat -- "$source" ;;
  esac
}

# Fetch a binary to a destination path.
fetch_file() {
  local source="$1" dest="$2"
  case "$source" in
    http://*|https://*)
      log "downloading ${source}"
      curl -fLsS --retry 3 --retry-delay 2 -o "$dest" "$source"
      ;;
    file://*)
      cp -- "${source#file://}" "$dest"
      ;;
    *)
      [ -f "$source" ] || die "asset not found: ${source}"
      cp -- "$source" "$dest"
      ;;
  esac
}

default_asset_url() {
  local name="$1"
  printf '%s/v%s/%s' "${ASSET_BASE_URL%/}" "$VERSION" "$name"
}

# Resolve the expected sha256 for one asset, refusing when none is available.
expected_sha() {
  local component="$1" asset="$2" explicit="$3"
  if [ -n "$explicit" ]; then
    printf '%s' "$explicit"
    return 0
  fi
  local checksums=""
  if [ -n "$CHECKSUMS_FILE" ]; then
    checksums="$(fetch_text "$CHECKSUMS_FILE")" || return 1
  elif [ -n "$CHECKSUMS_URL" ]; then
    checksums="$(fetch_text "$CHECKSUMS_URL")" || return 1
  else
    local url="${ASSET_BASE_URL%/}/v${VERSION}/beszel_${VERSION}_checksums.txt"
    checksums="$(fetch_text "$url")" || return 1
  fi
  local match
  match="$(printf '%s\n' "$checksums" | awk -v a="$asset" '$2==a || $2=="*"a {print $1; exit}')"
  [ -n "$match" ] || return 1
  printf '%s' "$match"
}

verify_and_install_asset() {
  local component="$1" asset="$2" asset_url="$3" bin_name="$4" explicit_sha="$5"
  local expected actual tarball dest
  expected="$(expected_sha "$component" "$asset" "$explicit_sha")" \
    || die "no sha256 for ${asset}; refusing to install an unverified ${component} binary"
  [[ "$expected" =~ ^[0-9A-Fa-f]{64}$ ]] || die "checksum for ${asset} is not 64 hex characters"

  tarball="${TMP_DIR}/${asset}"
  fetch_file "$asset_url" "$tarball"
  actual="$(sha256sum "$tarball" | awk '{print $1}')"
  [ "$actual" = "$(printf '%s' "$expected" | tr 'A-F' 'a-f')" ] \
    || die "sha256 mismatch for ${asset} (expected ${expected}, got ${actual})"
  log "sha256 verified for ${asset}"

  tar -xzf "$tarball" -C "$TMP_DIR"
  [ -f "${TMP_DIR}/${bin_name}" ] \
    || die "malformed ${asset}: ${bin_name} not found inside"
  install -d -m 0755 "$INSTALL_DIR"
  install -m 0755 "${TMP_DIR}/${bin_name}" "${INSTALL_DIR}/${bin_name}"
  log "installed ${INSTALL_DIR}/${bin_name}"
}

install_systemd_units() {
  local unit src user
  install -d -m 0755 "$DATA_DIR" "${DATA_DIR}-agent"
  # The hub runs as `beszel` and writes its PocketBase data dir here; the
  # installer must hand it ownership or the service cannot start.
  user="${BESZEL_USER:-beszel}"
  if id -u "$user" >/dev/null 2>&1; then
    chown "$user:$user" "$DATA_DIR" "${DATA_DIR}-agent" 2>/dev/null || true
  else
    log "WARNING: user ${user} does not exist; create it before starting the services"
  fi
  for unit in beszel-hub.service beszel-agent.service; do
    case "$unit" in
      beszel-hub.service)   [ "$COMPONENT" = hub ]   || [ "$COMPONENT" = both ] || continue ;;
      beszel-agent.service) [ "$COMPONENT" = agent ] || [ "$COMPONENT" = both ] || continue ;;
    esac
    # The units are shipped by the repo next to this script's deploy/ tree.
    src="${BESZEL_UNIT_DIR:-${SCRIPT_DIR}/../systemd}/${unit}"
    if [ -f "$src" ]; then
      install -m 0644 "$src" "${SYSTEMD_DIR}/${unit}"
      log "installed ${SYSTEMD_DIR}/${unit}"
    else
      die "systemd unit not found: ${src} (run from a checkout, or pass --no-systemd)"
    fi
  done
  if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload
    # Enable (not start): the units come back after a reboot, but the operator
    # configures /etc/jiuyue/beszel.env first. See docs/monitoring.md §8.
    for unit in beszel-hub.service beszel-agent.service; do
      case "$unit" in
        beszel-hub.service)   [ "$COMPONENT" = hub ]   || [ "$COMPONENT" = both ] || continue ;;
        beszel-agent.service) [ "$COMPONENT" = agent ] || [ "$COMPONENT" = both ] || continue ;;
      esac
      systemctl enable "$unit" 2>/dev/null || log "WARNING: could not enable ${unit}"
    done
  fi
}

main() {
  parse_args "$@"
  need_cmd curl
  need_cmd sha256sum
  need_cmd tar
  need_cmd install

  TMP_DIR="$(mktemp -d)"

  local hub_asset agent_asset
  # Beszel release asset names carry NO version (only the .deb and the
  # checksums file do): `beszel_linux_amd64.tar.gz`, `beszel-agent_linux_amd64.tar.gz`.
  hub_asset="beszel_linux_${ARCH}.tar.gz"
  agent_asset="beszel-agent_linux_${ARCH}.tar.gz"

  if [ "$COMPONENT" = hub ] || [ "$COMPONENT" = both ]; then
    [ -n "$HUB_ASSET_URL" ] || HUB_ASSET_URL="$(default_asset_url "$hub_asset")"
    verify_and_install_asset hub "$hub_asset" "$HUB_ASSET_URL" beszel "$SHA256_HUB"
  fi
  if [ "$COMPONENT" = agent ] || [ "$COMPONENT" = both ]; then
    [ -n "$AGENT_ASSET_URL" ] || AGENT_ASSET_URL="$(default_asset_url "$agent_asset")"
    verify_and_install_asset agent "$agent_asset" "$AGENT_ASSET_URL" beszel-agent "$SHA256_AGENT"
  fi

  if [ "$INSTALL_SYSTEMD" -eq 1 ]; then
    install_systemd_units
    log "units installed and enabled; set /etc/jiuyue/beszel.env then: systemctl restart beszel-hub beszel-agent"
  else
    log "systemd installation skipped (--no-systemd)"
  fi
  log "done (beszel ${VERSION}, component=${COMPONENT})"
}

main "$@"
