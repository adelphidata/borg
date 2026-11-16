#!/usr/bin/env bash
# Property-based test of the desktop surface.
#
#   run-desktop.sh test [bombadil flags…]  explore for PBT_TIME_LIMIT (default 3m)
#   run-desktop.sh reproduce <trace-dir>   replay a run's trace, same options
#   run-desktop.sh shrink <trace-dir>      search for a shorter failing trace
#   run-desktop.sh inspect [trace-dir]     open the trace viewer
#
# Runs the built app (apps/desktop/dist + dist-electron; built when missing,
# PBT_BUILD=1 forces it) in its own Electron instance with its own profile,
# pointed at a scratch borgd over the unix socket, and lets Bombadil drive it
# through the remote debugger. Exit code 2 means a property was violated.
set -euo pipefail
# shellcheck source=lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

command=${1:-test}
shift || true
spec="$PBT_DIR/spec/desktop.ts"
out="$OUTPUT_DIR/desktop"
app="$REPO_ROOT/apps/desktop"
mkdir -p "$OUTPUT_DIR"

# The viewer needs neither the daemon nor the app.
if [[ "$command" == inspect ]]; then
  exec "$BOMBADIL" browser inspect "${1:-$out}"
fi

ensure_binaries
prepare_home
start_daemon

if [[ "${PBT_BUILD:-0}" == 1 || ! -f "$app/dist/index.html" || ! -f "$app/dist-electron/main.js" ]]; then
  log "building the desktop app"
  bun run --cwd "$app" build >"$PBT_ROOT/desktop-build.log" 2>&1 || die "desktop build failed; see $PBT_ROOT/desktop-build.log"
fi

electron="$app/node_modules/.bin/electron"
[[ -x "$electron" ]] || electron="$REPO_ROOT/node_modules/.bin/electron"
[[ -x "$electron" ]] || die "electron is not installed; run bun install at the repo root"

port=${PBT_DEBUG_PORT:-9444}
if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
  die "port $port is busy; set PBT_DEBUG_PORT"
fi

# Own profile, so the single-instance lock and window state never touch the
# user's app; BORG_HOME points the socket probe at the scratch daemon, and the
# global hotkey stays unregistered so the user's instance keeps it.
# shellcheck disable=SC2046
env $(credential_unsets) \
  BORG_HOME="$PBT_HOME" \
  BORG_KEK_PASSPHRASE="$PBT_KEK_PASSPHRASE" \
  BORG_DAEMON_BIN="$(borgd_bin)" \
  BORG_DESKTOP_NO_GLOBAL_HOTKEY=1 \
  "$electron" "$app" --remote-debugging-port="$port" \
  --user-data-dir="$PBT_ROOT/electron-data" >"$PBT_ROOT/electron.log" 2>&1 &
ELECTRON_PID=$!
for _ in $(seq 1 60); do
  if curl -sf -o /dev/null "http://127.0.0.1:$port/json/version"; then break; fi
  kill -0 "$ELECTRON_PID" 2>/dev/null || die "electron exited; see $PBT_ROOT/electron.log"
  sleep 0.5
done
curl -sf -o /dev/null "http://127.0.0.1:$port/json/version" || die "electron never opened its debugger on :$port"

origin="file://$app/dist/index.html"
flags=(--width "${PBT_WIDTH:-1024}" --height "${PBT_HEIGHT:-768}")
external=(--remote-debugger "http://127.0.0.1:$port")

case "$command" in
  test)
    args=(browser test-external "${flags[@]}" "${external[@]}" --output-path "$out" --output-path-overwrite)
    args+=(--time-limit "$PBT_TIME_LIMIT")
    [[ "${PBT_EXIT_ON_VIOLATION:-1}" == 1 ]] && args+=(--exit-on-violation)
    log "exploring $origin for $PBT_TIME_LIMIT"
    "$BOMBADIL" "${args[@]}" "$@" "$origin" "$spec"
    ;;
  reproduce)
    trace=${1:?usage: run-desktop.sh reproduce <trace-dir>}
    shift
    "$BOMBADIL" browser test-external "${flags[@]}" "${external[@]}" --reproduce "$trace" "$@" "$origin" "$spec"
    ;;
  shrink)
    die "shrink needs a browser Bombadil manages itself; shrink the same trace with run-web.sh when the bug is in shared code"
    ;;
  *)
    die "unknown command: $command (test | reproduce | inspect)"
    ;;
esac
