#!/usr/bin/env bash
# Exercise deploy-host.sh against a fake release. No network and no systemd.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
script="${root}/scripts/deploy-host.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

fail() {
  printf 'FAIL %s\n' "$*" >&2
  exit 1
}

assert_eq() {
  [[ "$1" == "$2" ]] || fail "expected [$2] got [$1] ($3)"
}

run_rc() {
  set +e
  "$@" >/dev/null
  rc=$?
  set -e
  printf '%s' "$rc"
}

# --- command boundary -------------------------------------------------------
rc="$(run_rc bash "$script" 'deploy v1.2.3; id')"
assert_eq "$rc" 2 "shell metacharacters"
rc="$(run_rc bash "$script" 'deploy v1.2.3 && id')"
assert_eq "$rc" 2 "shell operator"
rc="$(run_rc bash "$script" 'deploy v1.2')"
assert_eq "$rc" 2 "short tag"
rc="$(run_rc env SSH_ORIGINAL_COMMAND='deploy v1.2.3 extra' bash "$script")"
assert_eq "$rc" 2 "extra argument"
rc="$(run_rc env SSH_ORIGINAL_COMMAND='deploy v1.2.3' UMSP_DEPLOY_ARCHIVE=/tmp/x bash "$script")"
assert_eq "$rc" 2 "remote archive override"

write_release() {
  local dir="$1" version="$2" unit_body="$3" bin_body="$4"
  local name="umsp-${version}-x86_64-unknown-linux-musl"
  mkdir -p "${dir}/${name}"
  printf '%s\n' "$bin_body" > "${dir}/${name}/umsp"
  chmod 755 "${dir}/${name}/umsp"
  printf '%s\n' "$unit_body" > "${dir}/${name}/umsp.service"
  printf '%s\n' '#!/bin/sh' > "${dir}/${name}/umsp-deploy"
  chmod 755 "${dir}/${name}/umsp-deploy"
  tar -C "$dir" -czf "${dir}/${name}.tar.gz" "$name"
  (
    cd "$dir"
    sha256sum "${name}.tar.gz" > "${name}.tar.gz.sha256"
  )
}

bin_for() {
  local version="$1"
  cat <<EOF
#!/bin/sh
case "\$1" in
  version) printf '%s\n' '{"ok":true,"version":"${version}","target":"t","profile":"release","os":"linux","arch":"x86_64"}' ;;
  *) exit 1 ;;
esac
EOF
}

install_fakes() {
  local health_file="$1" sys_log="$2"
  cat > "${work}/curl" <<EOF
#!/bin/sh
if [ -f "${work}/curl.fail" ]; then
  exit 22
fi
cat "${health_file}"
EOF
  cat > "${work}/systemctl" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >> "${sys_log}"
exit 0
EOF
  chmod 755 "${work}/curl" "${work}/systemctl"
}

common_env=(
  "UMSP_BIN_DIR=${work}/bin"
  "UMSP_UNIT_PATH=${work}/unit/umsp.service"
  "UMSP_STATE_DIR=${work}/state"
  "UMSP_DEPLOY_REQUIRE_ELF=0"
  "UMSP_CURL=${work}/curl"
  "UMSP_SYSTEMCTL=${work}/systemctl"
  "UMSP_HEALTH_URL=http://fake/healthz"
  "UMSP_HEALTH_ATTEMPTS=3"
  "HOME=${work}/home"
)
mkdir -p "${work}/bin" "${work}/unit" "${work}/state" "${work}/home"

# --- happy path, then unchanged skip ----------------------------------------
happy="${work}/happy"
write_release "$happy" "1.2.3" "[Service]\nExecStart=/bin/true\n" "$(bin_for 1.2.3)"
printf '%s\n' '{"status":"ok","version":"1.2.3","uptime_s":1}' > "${work}/health.json"
install_fakes "${work}/health.json" "${work}/sys.log"
printf '%s\n' 'OLD' > "${work}/bin/umsp"
chmod 755 "${work}/bin/umsp"
printf '%s\n' '[Service]' > "${work}/unit/umsp.service"

env "${common_env[@]}" UMSP_DEPLOY_ARCHIVE="${happy}/umsp-1.2.3-x86_64-unknown-linux-musl.tar.gz" \
  bash "$script" deploy v1.2.3
grep -q 'daemon-reload' "${work}/sys.log" || fail "unit change did not reload"
grep -q 'restart umsp.service' "${work}/sys.log" || fail "did not restart"
grep -q 'deployed v1.2.3' "${work}/state/deploy.log" || fail "missing deployed log"
installed="$(sha256sum "${work}/bin/umsp" | awk '{print $1}')"
stamp="$(tr -d '[:space:]' < "${work}/state/deployed.sha")"
assert_eq "$installed" "$stamp" "stamp"
# previous bytes kept for rollback
grep -q 'OLD' "${work}/bin/umsp.bak" || fail "backup missing"

: > "${work}/sys.log"
env "${common_env[@]}" UMSP_DEPLOY_ARCHIVE="${happy}/umsp-1.2.3-x86_64-unknown-linux-musl.tar.gz" \
  bash "$script" deploy v1.2.3
[[ ! -s "${work}/sys.log" ]] || fail "unchanged deploy restarted: $(cat "${work}/sys.log")"
grep -q 'unchanged v1.2.3' "${work}/state/deploy.log" || fail "missing unchanged log"

# --- health failure rolls back ----------------------------------------------
bad="${work}/bad"
write_release "$bad" "1.2.4" "[Service]\nExecStart=/bin/new\n" "$(bin_for 1.2.4)"
printf '%s\n' 'CURRENT' > "${work}/bin/umsp"
chmod 755 "${work}/bin/umsp"
printf '%s\n' 'CURRENT-UNIT' > "${work}/unit/umsp.service"
rm -f "${work}/state/deployed.sha"
touch "${work}/curl.fail"
: > "${work}/sys.log"
rc="$(run_rc env "${common_env[@]}" UMSP_DEPLOY_ARCHIVE="${bad}/umsp-1.2.4-x86_64-unknown-linux-musl.tar.gz" \
  bash "$script" deploy v1.2.4)"
assert_eq "$rc" 1 "rollback exit"
assert_eq "$(cat "${work}/bin/umsp")" "CURRENT" "binary restored"
assert_eq "$(cat "${work}/unit/umsp.service")" "CURRENT-UNIT" "unit restored"
grep -q 'rolled back' "${work}/state/deploy.log" || fail "missing rollback log"
rm -f "${work}/curl.fail"

# --- version mismatch never swaps -------------------------------------------
wrong="${work}/wrong"
write_release "$wrong" "9.9.9" "[Service]\nX=1\n" "$(bin_for 1.0.0)"
# archive name follows the tag we will pass; rebuild under the tag's version
# with a binary that reports a different version.
name="umsp-3.0.0-x86_64-unknown-linux-musl"
mkdir -p "${work}/wrongpack/${name}"
printf '%s\n' "$(bin_for 1.0.0)" > "${work}/wrongpack/${name}/umsp"
chmod 755 "${work}/wrongpack/${name}/umsp"
printf '%s\n' '[Service]' > "${work}/wrongpack/${name}/umsp.service"
tar -C "${work}/wrongpack" -czf "${work}/wrongpack/${name}.tar.gz" "$name"
(cd "${work}/wrongpack" && sha256sum "${name}.tar.gz" > "${name}.tar.gz.sha256")
printf '%s\n' 'STAY' > "${work}/bin/umsp"
chmod 755 "${work}/bin/umsp"
: > "${work}/sys.log"
rc="$(run_rc env "${common_env[@]}" UMSP_DEPLOY_ARCHIVE="${work}/wrongpack/${name}.tar.gz" \
  bash "$script" deploy v3.0.0)"
assert_eq "$rc" 1 "version mismatch exit"
assert_eq "$(cat "${work}/bin/umsp")" "STAY" "version mismatch swapped"
[[ ! -s "${work}/sys.log" ]] || fail "version mismatch restarted"

# --- unexpected archive member ----------------------------------------------
evil="${work}/evil"
name="umsp-4.0.0-x86_64-unknown-linux-musl"
mkdir -p "${evil}/${name}"
printf '%s\n' "$(bin_for 4.0.0)" > "${evil}/${name}/umsp"
chmod 755 "${evil}/${name}/umsp"
printf '%s\n' '[Service]' > "${evil}/${name}/umsp.service"
printf '%s\n' pwned > "${evil}/${name}/pwned"
tar -C "$evil" -czf "${evil}/${name}.tar.gz" "$name"
(cd "$evil" && sha256sum "${name}.tar.gz" > "${name}.tar.gz.sha256")
printf '%s\n' 'STAY' > "${work}/bin/umsp"
rc="$(run_rc env "${common_env[@]}" UMSP_DEPLOY_ARCHIVE="${evil}/${name}.tar.gz" \
  bash "$script" deploy v4.0.0)"
assert_eq "$rc" 1 "evil member exit"
assert_eq "$(cat "${work}/bin/umsp")" "STAY" "evil member swapped"

# --- checksum mismatch ------------------------------------------------------
name="umsp-5.0.0-x86_64-unknown-linux-musl"
mkdir -p "${work}/sum/${name}"
printf '%s\n' "$(bin_for 5.0.0)" > "${work}/sum/${name}/umsp"
chmod 755 "${work}/sum/${name}/umsp"
printf '%s\n' '[Service]' > "${work}/sum/${name}/umsp.service"
tar -C "${work}/sum" -czf "${work}/sum/${name}.tar.gz" "$name"
printf '%s\n' "0000000000000000000000000000000000000000000000000000000000000000  ${name}.tar.gz" > "${work}/sum/${name}.tar.gz.sha256"
printf '%s\n' 'STAY' > "${work}/bin/umsp"
rc="$(run_rc env "${common_env[@]}" UMSP_DEPLOY_ARCHIVE="${work}/sum/${name}.tar.gz" \
  bash "$script" deploy v5.0.0)"
assert_eq "$rc" 1 "checksum exit"
assert_eq "$(cat "${work}/bin/umsp")" "STAY" "checksum swapped"

printf '%s\n' 'STAY' > "${work}/bin/umsp"
chmod 755 "${work}/bin/umsp"
rc="$(run_rc env "${common_env[@]}" UMSP_DEPLOY_REQUIRE_ELF=1 \
  UMSP_DEPLOY_ARCHIVE="${happy}/umsp-1.2.3-x86_64-unknown-linux-musl.tar.gz" \
  bash "$script" deploy v1.2.3)"
assert_eq "$rc" 1 "elf gate exit"
assert_eq "$(cat "${work}/bin/umsp")" "STAY" "elf gate swapped"

printf '%s\n' "deploy-host tests ok"
