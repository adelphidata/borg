#!/usr/bin/env bash
# Property-based test of the browser surface.
#
#   run-web.sh test [bombadil flags…]     explore for PBT_TIME_LIMIT (default 3m)
#   run-web.sh reproduce <trace-dir>      replay a run's trace, same options
#   run-web.sh shrink <trace-dir> [flags] search for a shorter failing trace
#   run-web.sh inspect [trace-dir]        open the trace viewer
#
# The SPA comes from a Vite dev server (current source, no Rust rebuild) that
# proxies /grpc to a scratch borgd. PBT_WEB_SERVER=gateway tests the SPA the
# daemon embeds instead. Exit code 2 means a property was violated.
set -euo pipefail
# shellcheck source=lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

command=${1:-test}
shift || true
spec="$PBT_DIR/spec/web.ts"
out="$OUTPUT_DIR/web"
mkdir -p "$OUTPUT_DIR"

# The viewer needs neither the daemon nor the app.
if [[ "$command" == inspect ]]; then
  [[ -x "$BOMBADIL" ]] || die "bombadil is not installed; run bun install at the repo root"
  exec "$BOMBADIL" browser inspect "${1:-$out}"
fi

ensure_binaries
pin_chrome
prepare_home
start_daemon
issue_token

if [[ "${PBT_WEB_SERVER:-vite}" == gateway ]]; then
  origin="http://127.0.0.1:$PBT_GATEWAY_PORT/ui/#/?token=$TOKEN"
else
  port=${PBT_WEB_PORT:-5184}
  if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
    die "port $port is busy; set PBT_WEB_PORT"
  fi
  (
    cd "$REPO_ROOT/apps/web"
    BORG_GATEWAY_PORT=$PBT_GATEWAY_PORT exec bun x vite --port "$port" --strictPort
  ) >"$PBT_ROOT/vite-web.log" 2>&1 &
  VITE_PID=$!
  for _ in $(seq 1 60); do
    if curl -sf -o /dev/null "http://localhost:$port/ui/"; then break; fi
    kill -0 "$VITE_PID" 2>/dev/null || die "vite exited; see $PBT_ROOT/vite-web.log"
    sleep 0.5
  done
  curl -sf -o /dev/null "http://localhost:$port/ui/" || die "vite never answered on :$port"
  origin="http://localhost:$port/ui/#/?token=$TOKEN"
fi

flags=()
while IFS= read -r flag; do flags+=("$flag"); done < <(browser_flags)

case "$command" in
  test)
    args=(browser test "${flags[@]}" --output-path "$out" --output-path-overwrite)
    args+=(--time-limit "$PBT_TIME_LIMIT")
    [[ "${PBT_EXIT_ON_VIOLATION:-1}" == 1 ]] && args+=(--exit-on-violation)
    log "exploring $origin for $PBT_TIME_LIMIT"
    "$BOMBADIL" "${args[@]}" "$@" "$origin" "$spec"
    ;;
  reproduce)
    trace=${1:?usage: run-web.sh reproduce <trace-dir>}
    shift
    "$BOMBADIL" browser test "${flags[@]}" --reproduce "$trace" "$@" "$origin" "$spec"
    ;;
  shrink)
    trace=${1:?usage: run-web.sh shrink <trace-dir> [--property NAME]}
    shift
    "$BOMBADIL" browser shrink "${flags[@]}" --trace "$trace" \
      --output-path "$out-shrunk" --output-path-overwrite "$@" "$origin" "$spec"
    ;;
  *)
    die "unknown command: $command (test | reproduce | shrink | inspect)"
    ;;
esac
