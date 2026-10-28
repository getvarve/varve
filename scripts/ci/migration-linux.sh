#!/usr/bin/env bash
# Upgrades a varve-community 0.1.7 deb install to the varve deb built from this commit, the way a
# user does with apt, and checks what is left (bundle.linux.deb conflicts and replaces in
# tauri.conf.json). CI runs it on a fresh runner (.github/workflows/ci.yml, Migration (linux)); it
# changes the machine.
#
# Usage: scripts/ci/migration-linux.sh OLD_DEB NEW_DEB
set -euo pipefail

old="${1:?usage: migration-linux.sh OLD_DEB NEW_DEB}"
new="${2:?usage: migration-linux.sh OLD_DEB NEW_DEB}"
data_dir="${XDG_DATA_HOME:-${HOME}/.local/share}/Varve"
marker="${data_dir}/migration-marker.txt"
failures=0

section() {
  printf '\n==== %s\n' "$1"
}

check() {
  local what="$1"
  shift
  if "$@"; then
    printf 'PASS  %s\n' "$what"
  else
    printf 'FAIL  %s\n' "$what"
    printf '::error::%s\n' "$what"
    failures=$((failures + 1))
  fi
}

installed() {
  [ "$(dpkg-query -W -f='${Status}' "$1" 2>/dev/null || true)" = "install ok installed" ]
}

not_installed() {
  ! installed "$1"
}

name_is_varve() {
  grep -qx 'Name=Varve' "$@"
}

show_state() {
  section "$1"
  dpkg-query -W -f='${Package} ${Version} ${Status}\n' 'varve*' 2>/dev/null || echo "(no varve packages)"
  ls -la /usr/bin/varve 2>/dev/null || echo "/usr/bin/varve (missing)"
  grep -H '^Name=' /usr/share/applications/*.desktop 2>/dev/null | grep -i varve || echo "(no Varve desktop entry)"
  ls -la "$data_dir" 2>/dev/null || echo "${data_dir} (missing)"
}

# apt only installs a local file when the path has a slash.
local_path() {
  case "$1" in
    */*) printf '%s\n' "$1" ;;
    *) printf './%s\n' "$1" ;;
  esac
}

show_state "Before"

section "Install varve-community 0.1.7"
sudo apt-get install -y "$(local_path "$old")"
check "varve-community is installed" installed varve-community
check "/usr/bin/varve exists" test -x /usr/bin/varve

section "App data marker"
mkdir -p "$data_dir"
echo "written before the upgrade" >"$marker"
echo "wrote ${marker}"

show_state "With varve-community 0.1.7"

section "Install varve from this commit"
sudo apt-get install -y "$(local_path "$new")"

show_state "After the upgrade"

section "Checks"
check "varve is installed" installed varve
check "varve-community is gone" not_installed varve-community
check "/usr/bin/varve exists" test -x /usr/bin/varve
check "/usr/bin/varve belongs to varve" sh -c 'dpkg -S /usr/bin/varve | grep -q "^varve:"'
mapfile -t desktop_files < <(dpkg -L varve | grep '\.desktop$' || true)
printf 'desktop entries: %s\n' "${desktop_files[*]:-(none)}"
check "the desktop entry is named Varve" name_is_varve "${desktop_files[@]:-/dev/null}"
check "the app data marker is still there" test -f "$marker"

section "Result"
if [ "$failures" -gt 0 ]; then
  echo "${failures} check(s) failed"
  exit 1
fi
echo "Every check passed (deb)."
