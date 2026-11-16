#!/usr/bin/env bash
# Shared plumbing for the Bombadil runners: a scratch daemon beside the real
# one, a webui token for it, and a pinned Chrome for Bombadil to launch.
#
# Sourced by run-web.sh and run-desktop.sh; not meant to be run on its own.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
PBT_DIR="$REPO_ROOT/packages/pbt"
# Short on purpose: the daemon's unix socket lives under here and socket
# paths have a hard length limit.
PBT_ROOT="${PBT_ROOT:-/tmp/borg-pbt}"
PBT_HOME="$PBT_ROOT/home"
PBT_GATEWAY_PORT="${PBT_GATEWAY_PORT:-7999}"
PBT_KEK_PASSPHRASE="${PBT_KEK_PASSPHRASE:-bombadil}"
PBT_TIME_LIMIT="${PBT_TIME_LIMIT:-3m}"
BOMBADIL="$REPO_ROOT/node_modules/.bin/bombadil"
OUTPUT_DIR="${PBT_OUTPUT_DIR:-$PBT_DIR/output}"

DAEMON_PID=""
VITE_PID=""
ELECTRON_PID=""
TOKEN=""

log() { printf '[pbt] %s\n' "$*" >&2; }
die() {
  log "$*"
  exit 1
}

borgd_bin() { echo "${BORG_DAEMON_BIN:-$REPO_ROOT/target/debug/borgd}"; }
borg_bin() { echo "${BORG_CLI_BIN:-$REPO_ROOT/target/debug/borg}"; }

ensure_binaries() {
  if [[ ! -x "$(borgd_bin)" || ! -x "$(borg_bin)" ]]; then
    log "building borg and borgd (debug)"
    (cd "$REPO_ROOT" && cargo build --bin borgd --bin borg)
  fi
  [[ -x "$BOMBADIL" ]] || die "bombadil is not installed; run bun install at the repo root"
  command -v sqlite3 >/dev/null 2>&1 || die "sqlite3 is required to seed the scratch home"
}

# `-u VAR` for every credential-looking variable, so the scratch daemon can
# neither detect a provider nor spend anyone's tokens.
credential_unsets() {
  env | grep -oE '^[A-Za-z0-9_]*(API_KEY|_TOKEN)=' | sed 's/=$//; s/^/-u /' | tr '\n' ' '
}

# A scratch BORG_HOME: file-backed key store (never the OS keychain), its own
# gateway port, onboarding already done, and a model endpoint that refuses
# connections. Kept between runs so traces reproduce; PBT_FRESH=1 wipes it.
prepare_home() {
  if [[ "${PBT_FRESH:-0}" == 1 ]]; then
    log "wiping $PBT_HOME"
    rm -rf "$PBT_HOME" "$PBT_ROOT/webui.token" "$PBT_ROOT/electron-data"
  fi
  [[ -f "$PBT_HOME/borg.db" ]] && return
  mkdir -p "$PBT_HOME"
  sqlite3 "$PBT_HOME/borg.db" <<SQL
CREATE TABLE IF NOT EXISTS settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL,
  updated_at INTEGER NOT NULL DEFAULT 0
);
INSERT OR REPLACE INTO settings(key, value) VALUES
  ('security.kek_backend', 'file'),
  ('gateway.port', '$PBT_GATEWAY_PORT'),
  ('onboarded', 'true'),
  ('llm.provider', 'custom'),
  ('llm.base_url', 'http://127.0.0.1:9/v1'),
  ('llm.model', 'pbt-stub');
SQL
  log "seeded scratch home at $PBT_HOME"
}

start_daemon() {
  if lsof -nP -iTCP:"$PBT_GATEWAY_PORT" -sTCP:LISTEN >/dev/null 2>&1; then
    if [[ -f "$PBT_HOME/borgd.pid" ]] && kill -0 "$(cat "$PBT_HOME/borgd.pid")" 2>/dev/null; then
      log "reusing the scratch daemon on :$PBT_GATEWAY_PORT"
      return
    fi
    die "port $PBT_GATEWAY_PORT is taken by something else; set PBT_GATEWAY_PORT"
  fi
  # shellcheck disable=SC2046
  env $(credential_unsets) \
    BORG_HOME="$PBT_HOME" \
    BORG_KEK_PASSPHRASE="$PBT_KEK_PASSPHRASE" \
    BORG_ACP_NO_AUTOSPAWN=1 \
    "$(borgd_bin)" >"$PBT_ROOT/borgd.log" 2>&1 &
  DAEMON_PID=$!
  for _ in $(seq 1 120); do
    if lsof -nP -iTCP:"$PBT_GATEWAY_PORT" -sTCP:LISTEN >/dev/null 2>&1; then
      log "scratch daemon up on :$PBT_GATEWAY_PORT (pid $DAEMON_PID)"
      return
    fi
    kill -0 "$DAEMON_PID" 2>/dev/null || die "borgd exited early; see $PBT_ROOT/borgd.log"
    sleep 0.5
  done
  die "borgd never opened the gateway; see $PBT_ROOT/borgd.log"
}

# One webui token per scratch home, kept on disk: the token is part of the
# origin URL, and a reproduction needs the same origin as the original run.
issue_token() {
  local token_file="$PBT_ROOT/webui.token" out
  if [[ -s "$token_file" ]]; then
    TOKEN=$(cat "$token_file")
    return
  fi
  out=$(
    # shellcheck disable=SC2046
    env $(credential_unsets) BORG_HOME="$PBT_HOME" BORG_KEK_PASSPHRASE="$PBT_KEK_PASSPHRASE" \
      BORG_ACP_NO_AUTOSPAWN=1 "$(borg_bin)" client-token issue "bombadil-$(date +%s)" --scopes webui 2>&1
  ) || die "could not issue a webui token: $out"
  TOKEN=$(printf '%s\n' "$out" | grep -oE '^[[:space:]]+[A-Za-z0-9_-]{30,}[[:space:]]*$' | tr -d '[:space:]')
  [[ -n "$TOKEN" ]] || die "could not read the token from: $out"
  (
    umask 077
    printf '%s' "$TOKEN" >"$token_file"
  )
}

chrome_path() {
  if [[ -n "${BOMBADIL_CHROME:-}" ]]; then
    echo "$BOMBADIL_CHROME"
    return
  fi
  local candidate
  for candidate in \
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
    "/Applications/Chromium.app/Contents/MacOS/Chromium"; do
    if [[ -x "$candidate" ]]; then
      echo "$candidate"
      return
    fi
  done
  for candidate in google-chrome google-chrome-stable chromium chromium-browser chrome; do
    if command -v "$candidate" >/dev/null 2>&1 && "$candidate" --version >/dev/null 2>&1; then
      command -v "$candidate"
      return
    fi
  done
  return 1
}

# Bombadil launches the first chromium/chrome it finds on PATH. Pin one, so a
# run, its reproduction and its shrink all use the same browser, and a stale
# launcher on PATH cannot break the run.
pin_chrome() {
  local chrome name
  chrome=$(chrome_path) || die "no Chrome or Chromium found; set BOMBADIL_CHROME"
  mkdir -p "$PBT_ROOT/bin"
  for name in chromium chrome google-chrome google-chrome-stable chromium-browser; do
    printf '#!/bin/sh\nexec %q "$@"\n' "$chrome" >"$PBT_ROOT/bin/$name"
    chmod +x "$PBT_ROOT/bin/$name"
  done
  export PATH="$PBT_ROOT/bin:$PATH"
  log "browser: $chrome"
}

# Flags every bombadil invocation shares. A reproduction or shrink must use
# the same ones as the original test, so they live in one place.
browser_flags() {
  [[ "${PBT_HEADED:-0}" == 1 ]] || printf -- '--headless\n'
  printf -- '--width\n%s\n--height\n%s\n' "${PBT_WIDTH:-1024}" "${PBT_HEIGHT:-768}"
}

cleanup() {
  local status=$?
  [[ -n "$VITE_PID" ]] && kill "$VITE_PID" 2>/dev/null || true
  [[ -n "$ELECTRON_PID" ]] && kill "$ELECTRON_PID" 2>/dev/null || true
  if [[ -n "$DAEMON_PID" && "${PBT_KEEP_DAEMON:-0}" != 1 ]]; then
    kill -TERM "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
  exit "$status"
}
trap cleanup EXIT INT TERM
