#!/usr/bin/env bash
# Runs the built client on the host. The host Wayland session is required,
# and lib/ bundles libxkbcommon-x11 (the atomic host image lacks it at
# runtime; the dev container only needs it to link).
set -euo pipefail
cd "$(dirname "$0")/.."

export LD_LIBRARY_PATH="$PWD/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
# The client's shipped default is the public rendezvous set. Point
# the dev loop at the local demo relay explicitly when you want it:
#   ORCHARD_RELAYS=ws://127.0.0.1:8008 ./scripts/run.sh
# An unset value inherits the shipped default; blank means the same
# thing (the client treats a blank value as no override). Publishing
# lands on the public relays regardless, so a dev run against only
# the local relay sees a month-stale catalog that looks like a
# sorting bug (the 2026-11-07 lesson).
export ORCHARD_RELAYS
# A dead relay fails silently: the client retries forever and the UI
# shows confidently stale catalog data (the 2026-11-07 lesson). Warn
# at launch when a configured relay is not accepting connections.
for relay in ${ORCHARD_RELAYS//,/ }; do
  host=${relay#*://}
  host=${host%%/*}
  if [[ "$host" == *:* ]]; then
    port=${host##*:}
  elif [[ "$relay" == wss://* ]]; then
    port=443
  else
    port=80
  fi
  if ! timeout 2 bash -c "exec 3<>/dev/tcp/${host%%:*}/$port" 2>/dev/null; then
    echo "WARNING: relay $relay is unreachable; the catalog will be" \
      "stale until it comes back (ORCHARD_RELAYS= runs the shipped" \
      "public set instead)" >&2
  fi
done
# exec would lose it: the exit status is the only forensic trace of
# a silent death. 0 is a clean exit (an in-code path to hunt), 137
# SIGKILL, 143 SIGTERM (relaunch housekeeping), 139 a segfault.
status=0
./target/release/orchard "$@" || status=$?
echo "orchard exited: $status" >&2
exit "$status"
