#!/usr/bin/env bash
# Installs apt packages on a GitHub Ubuntu runner without waiting on a dead mirror.
#
# The runner images list several mirrors in /etc/apt/apt-mirrors.txt, the Azure one first. When
# a mirror does not answer, apt waits 20 seconds on every single file before it tries the next
# one, which turns a one minute install into hours. This script asks each listed mirror for its
# release file first and keeps only the ones that answer. When none of them answers (Canonical's
# hosts have been unreachable from the runners as a group), it uses the independent mirrors
# below instead. apt still checks every package against Ubuntu's signing keys, so which mirror
# serves the files does not matter for what gets installed. When no mirror answers at all, it
# fails at once instead of hanging.
#
# Usage: scripts/ci/apt-install.sh PACKAGE [PACKAGE...]
set -euo pipefail

if (($# == 0)); then
  echo "usage: apt-install.sh PACKAGE [PACKAGE...]" >&2
  exit 2
fi

fallbacks=(
  https://mirrors.edge.kernel.org/ubuntu/
  https://mirror.leaseweb.com/ubuntu/
  https://mirrors.ocf.berkeley.edu/ubuntu/
)

# answers URL: whether the mirror serves the release file of this Ubuntu version.
answers() {
  curl --silent --fail --head --max-time 8 --output /dev/null "${1%/}/dists/${codename}/InRelease"
}

mirrors=/etc/apt/apt-mirrors.txt
if [[ -f "$mirrors" ]]; then
  codename="$(. /etc/os-release && echo "${VERSION_CODENAME:-}")"
  kept="$(mktemp)"
  while IFS= read -r line; do
    url="${line%%[[:space:]]*}"
    [[ -n "$url" ]] || continue
    if answers "$url"; then
      printf '%s\n' "$line" >>"$kept"
    else
      echo "apt-install.sh: mirror does not answer, skipping it: $url" >&2
    fi
  done <"$mirrors"
  if [[ ! -s "$kept" ]]; then
    echo "apt-install.sh: no listed mirror answers; trying independent mirrors" >&2
    priority=1
    for url in "${fallbacks[@]}"; do
      if answers "$url"; then
        printf '%s\tpriority:%d\n' "$url" "$priority" >>"$kept"
        priority=$((priority + 1))
      else
        echo "apt-install.sh: mirror does not answer, skipping it: $url" >&2
      fi
    done
  fi
  if [[ ! -s "$kept" ]]; then
    echo "apt-install.sh: no Ubuntu mirror answers from this runner" >&2
    exit 1
  fi
  echo "apt-install.sh: using these mirrors:" >&2
  cat "$kept" >&2
  sudo cp "$kept" "$mirrors"
fi

options=(-o Acquire::Retries=3 -o Acquire::http::Timeout=20 -o Acquire::https::Timeout=20)
sudo apt-get "${options[@]}" update
sudo apt-get "${options[@]}" install -y "$@"
