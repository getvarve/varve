#!/usr/bin/env bash
# Runs a command until it succeeds, at most ATTEMPTS times, waiting 15, 30, 60... seconds between
# tries. CI pulls its database images from Docker Hub, which now and then answers
# "toomanyrequests" for a moment; one retry is enough then.
#
# Usage: scripts/ci/retry.sh ATTEMPTS COMMAND [ARGS...]
set -euo pipefail

attempts="${1:?usage: retry.sh ATTEMPTS COMMAND [ARGS...]}"
shift
case "$attempts" in
  '' | *[!0-9]*) echo "retry.sh: ATTEMPTS must be a number" >&2; exit 2 ;;
esac

wait=15
for ((try = 1; ; try++)); do
  if "$@"; then
    exit 0
  fi
  if ((try >= attempts)); then
    echo "retry.sh: failed after $attempts attempts: $*" >&2
    exit 1
  fi
  echo "retry.sh: attempt $try of $attempts failed; trying again in ${wait}s" >&2
  sleep "$wait"
  wait=$((wait * 2))
done
