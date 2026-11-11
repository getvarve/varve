#!/usr/bin/env bash
# Installs apt packages on a GitHub Ubuntu runner without waiting on a dead mirror.
#
# The runner images list several mirrors in /etc/apt/apt-mirrors.txt, the Azure one first. When
# that mirror does not answer, apt waits 20 seconds on every single file before it tries the
# next mirror, which turns a one minute install into hours. This script asks each listed mirror
# for its release file first and removes the ones that do not answer, so apt only talks to
# mirrors that work. If none answers, the main Ubuntu archive is used.
#
# Usage: scripts/ci/apt-install.sh PACKAGE [PACKAGE...]
set -euo pipefail

if (($# == 0)); then
  echo "usage: apt-install.sh PACKAGE [PACKAGE...]" >&2
  exit 2
fi

mirrors=/etc/apt/apt-mirrors.txt
if [[ -f "$mirrors" ]]; then
  codename="$(. /etc/os-release && echo "${VERSION_CODENAME:-}")"
  kept="$(mktemp)"
  while IFS= read -r line; do
    url="${line%%[[:space:]]*}"
    [[ -n "$url" ]] || continue
    if curl --silent --fail --head --max-time 8 --output /dev/null "${url%/}/dists/${codename}/InRelease"; then
      printf '%s\n' "$line" >>"$kept"
    else
      echo "apt-install.sh: mirror does not answer, skipping it: $url" >&2
    fi
  done <"$mirrors"
  if [[ ! -s "$kept" ]]; then
    echo "apt-install.sh: no listed mirror answers; using archive.ubuntu.com" >&2
    printf 'http://archive.ubuntu.com/ubuntu/\tpriority:1\n' >"$kept"
  fi
  sudo cp "$kept" "$mirrors"
fi

options=(-o Acquire::Retries=3 -o Acquire::http::Timeout=20 -o Acquire::https::Timeout=20)
sudo apt-get "${options[@]}" update
sudo apt-get "${options[@]}" install -y "$@"
