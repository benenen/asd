#!/usr/bin/env bash
#
# share-gui-xpra.sh — run the asd GUI on a headless virtual display and share it
# over xpra (HTML5 + native client). Handy on a box with no monitor: the GUI
# renders through software Vulkan (lavapipe) and xpra forwards the frames.
#
#   scripts/share-gui-xpra.sh [start|stop|status|restart]
#
# Defaults can be overridden via env vars:
#   PORT=14711          TCP/HTML5 port xpra binds (0.0.0.0)
#   DISPLAY_NUM=100     virtual X display number (":100")
#   SESSION=demo        asd session name shown in the GUI
#   SOCKET=/tmp/asd-xpra.sock   daemon UDS (kept off $XDG_RUNTIME_DIR so the
#                               daemon and the GUI child always agree on it)
#   ASD=<auto>          path to the asd binary (defaults to target/release then
#                       target/debug under the repo)
#
# Because the GUI uses wgpu, it needs a Vulkan driver; on a headless host we
# point it at lavapipe (CPU). Override with VK_ICD if autodetection misses it.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

PORT="${PORT:-14711}"
DISPLAY_NUM="${DISPLAY_NUM:-100}"
SESSION="${SESSION:-demo}"
SOCKET="${SOCKET:-/tmp/asd-xpra.sock}"
DISP=":${DISPLAY_NUM}"

# Resolve the asd binary: prefer a release build, fall back to debug.
if [ -z "${ASD:-}" ]; then
  if   [ -x "$REPO/target/release/asd" ]; then ASD="$REPO/target/release/asd"
  elif [ -x "$REPO/target/debug/asd" ];   then ASD="$REPO/target/debug/asd"
  else
    echo "error: no asd binary found — build it first:" >&2
    echo "         cargo build --release            (from $REPO)" >&2
    exit 1
  fi
fi

# Resolve a Vulkan ICD for headless rendering (lavapipe = software Vulkan).
if [ -z "${VK_ICD:-}" ]; then
  for c in /usr/share/vulkan/icd.d/lvp_icd*.json; do
    [ -e "$c" ] && VK_ICD="$c" && break
  done
fi

export ASD_SOCKET="$SOCKET"
export WGPU_BACKEND="${WGPU_BACKEND:-vulkan}"
[ -n "${VK_ICD:-}" ] && export VK_ICD_FILENAMES="$VK_ICD"

# Best-effort LAN address for the access hint (SSH_CONNECTION destination, else
# the first private non-docker address).
lan_ip() {
  if [ -n "${SSH_CONNECTION:-}" ]; then
    awk '{print $3}' <<<"$SSH_CONNECTION"; return
  fi
  hostname -I 2>/dev/null | tr ' ' '\n' \
    | grep -E '^(10|192\.168)\.' | head -n1
}

xpra_running() { xpra list 2>/dev/null | grep -qE "LIVE session at ${DISP}\b"; }

ensure_session() {
  # `asd new` starts the daemon on demand and creates the session; skip if the
  # session already exists (re-running new with a taken name errors).
  if "$ASD" list 2>/dev/null | awk 'NR>1{print $1}' | grep -qx "$SESSION"; then
    echo "  session '$SESSION' already exists on $SOCKET"
  else
    echo "  creating session '$SESSION' (starts the daemon if needed)"
    "$ASD" new "$SESSION" \
      --cmd 'printf "asd GUI — shared via xpra\n"; exec "${SHELL:-/bin/bash}"' >/dev/null
  fi
}

print_access() {
  local ip; ip="$(lan_ip || true)"; ip="${ip:-<sandbox-ip>}"
  cat <<EOF

  ✅ asd GUI is shared on ${DISP} (bind 0.0.0.0:${PORT}, HTML5 on).

  This host is reachable only over SSH:22, so tunnel the port, then connect:

  A) Browser (HTML5, no install) — run on your machine:
       ssh -N -L ${PORT}:127.0.0.1:${PORT} <your-login>@${ip}
     then open:  http://localhost:${PORT}/

  B) Native xpra client (smoother; xpra tunnels over SSH itself):
       xpra attach ssh://<your-login>@${ip}/${DISPLAY_NUM}

  Stop sharing:  scripts/share-gui-xpra.sh stop
EOF
}

start() {
  if xpra_running; then
    echo "xpra already sharing on ${DISP} (port ${PORT})."
    print_access
    return 0
  fi
  # Refuse if something else holds the port (xpra would abort mid-init otherwise).
  if ss -ltn 2>/dev/null | grep -qE "[:.]${PORT}\b"; then
    echo "error: port ${PORT} is already in use — set PORT=<free-port> and retry." >&2
    exit 1
  fi

  echo "asd binary : $ASD"
  echo "vulkan ICD : ${VK_ICD:-<none found — GUI may fail without a GPU>}"
  echo "socket     : $SOCKET"
  echo "ensuring daemon + session..."
  ensure_session

  echo "starting xpra on ${DISP}..."
  # --start (not --start-child) so the server stays up even if the GUI exits.
  xpra start "$DISP" \
    --start="$ASD gui $SESSION" \
    --bind-tcp="0.0.0.0:${PORT}" --html=on --daemon=yes --sharing=yes \
    --env="ASD_SOCKET=${SOCKET}" \
    --env="WGPU_BACKEND=${WGPU_BACKEND}" \
    ${VK_ICD:+--env="VK_ICD_FILENAMES=${VK_ICD}"} \
    >/dev/null

  # Wait for the port to come up (xpra daemonizes immediately).
  for _ in $(seq 1 20); do
    ss -ltn 2>/dev/null | grep -qE "[:.]${PORT}\b" && break
    sleep 0.5
  done
  if ss -ltn 2>/dev/null | grep -qE "[:.]${PORT}\b"; then
    print_access
  else
    echo "error: xpra did not come up — see /run/xpra/${DISPLAY_NUM}/server.log" >&2
    exit 1
  fi
}

stop() {
  echo "stopping xpra on ${DISP} (the asd daemon/session keep running)..."
  xpra stop "$DISP" 2>/dev/null || echo "  (no xpra server on ${DISP})"
  echo "  to also drop the session: $ASD kill $SESSION"
}

status() {
  echo "=== xpra ==="
  xpra list 2>/dev/null | grep -E "${DISP}\b" || echo "  no xpra on ${DISP}"
  echo "=== port ${PORT} ==="
  ss -ltn 2>/dev/null | grep -E "[:.]${PORT}\b" || echo "  not listening"
  echo "=== asd sessions ($SOCKET) ==="
  "$ASD" list 2>&1 || true
}

case "${1:-start}" in
  start)   start ;;
  stop)    stop ;;
  restart) stop; sleep 1; start ;;
  status)  status ;;
  *) echo "usage: $0 [start|stop|status|restart]" >&2; exit 2 ;;
esac
