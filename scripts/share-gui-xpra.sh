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
#   MIN_QUALITY=70      floor for xpra's lossy encoder (see the note below)
#   IME_ENGINE=libpinyin        ibus engine used for Chinese input
#   IME_PACKAGE=ibus-libpinyin  package installed when that engine is missing
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
MIN_QUALITY="${MIN_QUALITY:-70}"
IME_ENGINE="${IME_ENGINE:-libpinyin}"
IME_PACKAGE="${IME_PACKAGE:-ibus-libpinyin}"
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

# Read the ibus environment for this display. xpra starts one ibus-daemon and
# one dbus session per display, so the addresses have to come from that daemon
# rather than from this shell.
ibus_env() {
  local pidfile="/run/xpra/${DISPLAY_NUM}/ibus-daemon.pid" pid
  [ -r "$pidfile" ] || return 1
  pid="$(cat "$pidfile")"
  [ -r "/proc/$pid/environ" ] || return 1
  tr '\0' '\n' < "/proc/$pid/environ" | grep -E '^DBUS_SESSION_BUS_ADDRESS=' | head -n1
}

# Make Chinese typing work on this display.
#
# xpra forwards key events, not composed text. A character composed by the
# viewer's own input method has no keycode in this display's keymap: it arrives
# as keycode 8 with NoSymbol and is dropped before any application sees it. So
# composition has to happen on this side, in the ibus that xpra starts.
ensure_ime() {
  # ibus scans /usr/share/ibus/component at startup, so the engine has to be
  # installed before the daemon that will serve it.
  if [ ! -e "/usr/share/ibus/component/${IME_ENGINE}.xml" ]; then
    echo "  installing ${IME_PACKAGE} (needed to type Chinese)"
    if ! DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$IME_PACKAGE" >/dev/null 2>&1; then
      echo "  warning: could not install ${IME_PACKAGE}; Chinese input will not work" >&2
      return 0
    fi
  fi

  local dbus=""
  for _ in $(seq 1 30); do
    dbus="$(ibus_env || true)"
    [ -n "$dbus" ] && break
    sleep 0.5
  done
  if [ -z "$dbus" ]; then
    echo "  warning: xpra's ibus never came up; Chinese input will not work" >&2
    return 0
  fi

  export DISPLAY="$DISP"
  export "${dbus?}"

  # An engine installed after this daemon started is invisible until it rescans.
  if ! ibus list-engine 2>/dev/null | grep -qE "^[[:space:]]+${IME_ENGINE} -"; then
    ibus restart >/dev/null 2>&1 || true
    sleep 3
  fi

  # Start in English. libpinyin otherwise boots in Chinese mode and swallows
  # ordinary shell input -- a plain "ls " comes back as Chinese words. A single
  # Shift tap switches to Chinese and back, which is the usual convention.
  if [ "$IME_ENGINE" = "libpinyin" ]; then
    gsettings set com.github.libpinyin.ibus-libpinyin.libpinyin init-chinese false 2>/dev/null || true
  fi
  gsettings set org.freedesktop.ibus.general preload-engines \
    "['xkb:us::eng', '${IME_ENGINE}']" 2>/dev/null || true
  ibus engine "$IME_ENGINE" >/dev/null 2>&1 || true

  # xpra runs ibus with --panel=disable, which leaves nothing to draw the
  # candidate list: the engine still composes, but blind. Run the stock panel so
  # homophones can be picked. ibus's own switch hotkey stays unused because xpra
  # does not forward Super.
  local panel_pid="/run/xpra/${DISPLAY_NUM}/ibus-panel.pid"
  if [ -r "$panel_pid" ] && kill -0 "$(cat "$panel_pid")" 2>/dev/null; then
    return 0
  fi
  setsid /usr/libexec/ibus-ui-gtk3 >/dev/null 2>&1 &
  echo $! > "$panel_pid"
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

  Chinese input: tap Shift to switch the terminal between English and pinyin.
                 Composition happens on this host, so the viewer's own input
                 method should stay off.

  Stop sharing:  scripts/share-gui-xpra.sh stop
EOF
}

start() {
  if xpra_running; then
    echo "xpra already sharing on ${DISP} (port ${PORT})."
    ensure_ime
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
  # --clipboard both directions so a copy inside the GUI (which writes the
  # server-side clipboard) reaches the viewing client's clipboard.
  # --min-quality floors the lossy encoder. xpra's own default is 1, so when a
  # viewer falls behind acknowledging damage it compresses the picture until
  # the backlog clears — measured down to quality 4 against a 1.75s
  # acknowledgement latency, which turns terminal text to mush. With a floor it
  # sheds frame rate instead, and the text stays readable.
  xpra start "$DISP" \
    --start="$ASD gui $SESSION" \
    --bind-tcp="0.0.0.0:${PORT}" --html=on --daemon=yes --sharing=yes \
    --clipboard=yes --clipboard-direction=both \
    --min-quality="${MIN_QUALITY}" \
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
    echo "setting up Chinese input..."
    ensure_ime
    print_access
  else
    echo "error: xpra did not come up — see /run/xpra/${DISPLAY_NUM}/server.log" >&2
    exit 1
  fi
}

stop() {
  echo "stopping xpra on ${DISP} (the asd daemon/session keep running)..."
  local panel_pid="/run/xpra/${DISPLAY_NUM}/ibus-panel.pid"
  if [ -r "$panel_pid" ]; then
    kill "$(cat "$panel_pid")" 2>/dev/null || true
    rm -f "$panel_pid"
  fi
  xpra stop "$DISP" 2>/dev/null || echo "  (no xpra server on ${DISP})"
  echo "  to also drop the session: $ASD kill $SESSION"
}

status() {
  echo "=== xpra ==="
  xpra list 2>/dev/null | grep -E "${DISP}\b" || echo "  no xpra on ${DISP}"
  echo "=== port ${PORT} ==="
  ss -ltn 2>/dev/null | grep -E "[:.]${PORT}\b" || echo "  not listening"
  echo "=== input method ==="
  local dbus; dbus="$(ibus_env || true)"
  if [ -z "$dbus" ]; then
    echo "  no ibus on ${DISP}"
  else
    ( export DISPLAY="$DISP"; export "${dbus?}"
      echo "  engine: $(ibus engine 2>/dev/null || echo '<none active>')" )
  fi
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
