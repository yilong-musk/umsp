#!/usr/bin/env bash
# Install one published UMSP GitHub Release on this host.
#
# Remote command, and the only command the deploy key may run:
#   deploy vX.Y.Z
# Local maintenance:
#   preflight
#
# The script swaps ~/.local/bin/umsp and, when the unit file changed,
# ~/.config/systemd/user/umsp.service. It does not read or write
# config.toml, the env file, or sessions/. A failed health check restores
# the previous binary and unit.
set -euo pipefail

REPO_PATH="${UMSP_RELEASE_REPO:-yilong-musk/umsp}"
TRIPLE="${UMSP_RELEASE_TRIPLE:-x86_64-unknown-linux-musl}"
BIN_DIR="${UMSP_BIN_DIR:-${HOME}/.local/bin}"
UNIT_PATH="${UMSP_UNIT_PATH:-${HOME}/.config/systemd/user/umsp.service}"
STATE_DIR="${UMSP_STATE_DIR:-${HOME}/.local/share/umsp}"
HEALTH_URL="${UMSP_HEALTH_URL:-http://127.0.0.1:3460/healthz}"
HEALTH_ATTEMPTS="${UMSP_HEALTH_ATTEMPTS:-25}"
CURL="${UMSP_CURL:-curl}"
SYSTEMCTL="${UMSP_SYSTEMCTL:-systemctl}"
REQUIRE_ELF="${UMSP_DEPLOY_REQUIRE_ELF:-1}"
DOWNLOAD_ATTEMPTS="${UMSP_DOWNLOAD_ATTEMPTS:-5}"

log() {
  mkdir -p "$STATE_DIR"
  printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" | tee -a "${STATE_DIR}/deploy.log" >&2
}

die() {
  log "$*"
  exit 1
}

# Accept only the two commands above. The match is the security boundary
# for the forced SSH command, so it must be a full-string match.
parse_request() {
  local raw="$1"
  case "$raw" in
    preflight)
      printf '%s\n' preflight
      ;;
    deploy\ v[0-9]*.[0-9]*.[0-9]*)
      if [[ "$raw" =~ ^deploy[[:space:]]+(v[0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
        printf '%s\n' "${BASH_REMATCH[1]}"
      else
        return 2
      fi
      ;;
    *)
      return 2
      ;;
  esac
}

runtime_env() {
  export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
  export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=${XDG_RUNTIME_DIR}/bus}"
}

preflight() {
  runtime_env
  command -v "$CURL" >/dev/null
  command -v jq >/dev/null
  command -v "$SYSTEMCTL" >/dev/null
  test -d "$BIN_DIR"
  test -w "$BIN_DIR"
  "$SYSTEMCTL" --user show umsp.service -p Id --value >/dev/null
  "$CURL" -fsS --max-time 5 "$HEALTH_URL" | jq -e '.status == "ok" and .version != ""' >/dev/null
  log "preflight ok"
}

download() {
  local url="$1" dest="$2" i
  for i in $(seq 1 "$DOWNLOAD_ATTEMPTS"); do
    if "$CURL" -fsSL --max-time 60 -o "$dest" "$url"; then
      return 0
    fi
    sleep $((i * 2))
  done
  return 1
}

assert_members() {
  local archive="$1" root="$2" entry
  while IFS= read -r entry; do
    [[ -z "$entry" ]] && continue
    case "$entry" in
      /*|*..*) die "archive path refused: ${entry}" ;;
    esac
    case "$entry" in
      "${root}/" | "${root}/umsp" | "${root}/umsp.service" | "${root}/umsp-deploy" | \
        "${root}/config.example.toml" | "${root}/README.md") ;;
      *) die "archive member refused: ${entry}" ;;
    esac
  done < <(tar -tzf "$archive")
}

assert_binary() {
  local path="$1" desc
  [[ -f "$path" && ! -L "$path" ]] || die "release binary missing"
  if [[ "$REQUIRE_ELF" == 1 ]]; then
    desc="$(file -b "$path")"
    case "$desc" in
      *ELF*\ statically\ linked* | *ELF*\ static-pie\ linked*) ;;
      *) die "release binary is not a static ELF: ${desc}" ;;
    esac
  fi
}

health_matches() {
  local expect="$1" body hv up st
  body="$("$CURL" -fsS --max-time 2 "$HEALTH_URL")" || return 1
  hv="$(printf '%s' "$body" | jq -r '.version // empty')"
  up="$(printf '%s' "$body" | jq -r '.uptime_s // empty')"
  st="$(printf '%s' "$body" | jq -r '.status // empty')"
  [[ "$hv" == "$expect" && "$st" == "ok" && "$up" =~ ^[0-9]+$ && "$up" -lt 60 ]]
}

wait_healthy() {
  local expect="$1" i
  for i in $(seq 1 "$HEALTH_ATTEMPTS"); do
    if health_matches "$expect"; then
      return 0
    fi
    sleep 0.4
  done
  return 1
}

restore_previous() {
  local backup_bin="$1" unit_backed="$2"
  if [[ -n "$backup_bin" && -f "$backup_bin" ]]; then
    install -m 0755 "$backup_bin" "${BIN_DIR}/umsp.restore"
    mv -f "${BIN_DIR}/umsp.restore" "${BIN_DIR}/umsp"
  fi
  if [[ "$unit_backed" == 1 && -f "${UNIT_PATH}.bak" ]]; then
    install -m 0644 "${UNIT_PATH}.bak" "${UNIT_PATH}.restore"
    mv -f "${UNIT_PATH}.restore" "$UNIT_PATH"
    "$SYSTEMCTL" --user daemon-reload
  fi
  "$SYSTEMCTL" --user restart umsp.service || true
}

apply_stage() {
  local tag="$1" stage="$2"
  local expect backup_bin="" unit_backed=0 new_sha old_sha stamp
  expect="${tag#v}"

  [[ ! -L "${stage}/umsp" && ! -L "${stage}/umsp.service" ]] || die "release archive contains a symlink"
  assert_binary "${stage}/umsp"
  got="$("${stage}/umsp" version --json | jq -er '.version')"
  [[ "$got" == "$expect" ]] || die "binary version ${got} != ${expect}"

  new_sha="$(sha256sum "${stage}/umsp" | awk '{print $1}')"
  if [[ -f "${BIN_DIR}/umsp" && -f "${STATE_DIR}/deployed.sha" ]]; then
    old_sha="$(sha256sum "${BIN_DIR}/umsp" | awk '{print $1}')"
    stamp="$(tr -d '[:space:]' < "${STATE_DIR}/deployed.sha")"
    if [[ "$old_sha" == "$new_sha" && "$stamp" == "$new_sha" ]]; then
      if "$CURL" -fsS --max-time 5 "$HEALTH_URL" | jq -e --arg v "$expect" '.status == "ok" and .version == $v' >/dev/null; then
        log "unchanged ${tag}"
        return 0
      fi
    fi
  fi

  mkdir -p "$BIN_DIR"
  if [[ -f "${BIN_DIR}/umsp" ]]; then
    backup_bin="${BIN_DIR}/umsp.bak"
    cp -a "${BIN_DIR}/umsp" "$backup_bin"
  fi
  install -m 0755 "${stage}/umsp" "${BIN_DIR}/umsp.new"
  mv -f "${BIN_DIR}/umsp.new" "${BIN_DIR}/umsp"

  if [[ ! -f "$UNIT_PATH" ]] || ! cmp -s "${stage}/umsp.service" "$UNIT_PATH"; then
    if [[ -f "$UNIT_PATH" ]]; then
      cp -a "$UNIT_PATH" "${UNIT_PATH}.bak"
      unit_backed=1
    fi
    install -Dm644 "${stage}/umsp.service" "$UNIT_PATH"
    "$SYSTEMCTL" --user daemon-reload
  fi

  if ! "$SYSTEMCTL" --user restart umsp.service || ! wait_healthy "$expect"; then
    log "health check failed; restoring previous binary"
    restore_previous "$backup_bin" "$unit_backed"
    die "deploy of ${tag} rolled back"
  fi

  printf '%s\n' "$new_sha" > "${STATE_DIR}/deployed.sha"
  if [[ -f "${stage}/umsp-deploy" ]] && bash -n "${stage}/umsp-deploy"; then
    install -m 0755 "${stage}/umsp-deploy" "${BIN_DIR}/umsp-deploy.new"
    mv -f "${BIN_DIR}/umsp-deploy.new" "${BIN_DIR}/umsp-deploy"
  fi
  log "deployed ${tag}"
}

deploy_tag() {
  local tag="$1" ver archive_name url_base tmp archive
  runtime_env
  command -v "$CURL" >/dev/null || die "curl missing"
  command -v jq >/dev/null || die "jq missing"
  mkdir -p "$STATE_DIR" "$BIN_DIR"
  exec 9>"${STATE_DIR}/deploy.lock"
  flock -w 180 9 || die "deploy lock held"

  ver="${tag#v}"
  archive_name="umsp-${ver}-${TRIPLE}.tar.gz"
  tmp="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf ${tmp@Q}" EXIT
  if [[ -n "${UMSP_DEPLOY_ARCHIVE:-}" ]]; then
    archive="$UMSP_DEPLOY_ARCHIVE"
    [[ -z "${SSH_ORIGINAL_COMMAND+x}" ]] || die "archive override refused"
    cp -a "$archive" "${tmp}/${archive_name}"
    cp -a "${archive}.sha256" "${tmp}/${archive_name}.sha256"
    archive="${tmp}/${archive_name}"
  else
    url_base="https://github.com/${REPO_PATH}/releases/download/${tag}"
    archive="${tmp}/${archive_name}"
    download "${url_base}/${archive_name}" "$archive" || die "download failed: ${archive_name}"
    download "${url_base}/${archive_name}.sha256" "${archive}.sha256" || die "download failed: checksum"
  fi
  (
    cd "$tmp"
    sha256sum -c "${archive_name}.sha256"
  ) || die "checksum mismatch"
  assert_members "$archive" "umsp-${ver}-${TRIPLE}"
  tar -xzf "$archive" -C "$tmp"
  apply_stage "$tag" "${tmp}/umsp-${ver}-${TRIPLE}"
}

main() {
  local raw parsed
  if [[ -n "${SSH_ORIGINAL_COMMAND+x}" ]]; then
    raw="$SSH_ORIGINAL_COMMAND"
    if [[ -n "${UMSP_DEPLOY_ARCHIVE:-}" ]]; then
      printf '%s\n' "usage: deploy vX.Y.Z" >&2
      exit 2
    fi
  else
    raw="$*"
  fi
  if ! parsed="$(parse_request "$raw")"; then
    printf '%s\n' "usage: deploy vX.Y.Z" >&2
    exit 2
  fi
  if [[ "$parsed" == preflight ]]; then
    preflight
    return 0
  fi
  deploy_tag "$parsed"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
