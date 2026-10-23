#!/bin/zsh
# Swap the running daemon on ~/.borg for the borgd cargo just built. `just
# daemon-watch` runs this after every build. Only a borgd from this repo's
# target/debug is ever stopped, and a failed build never gets here, so the old
# daemon keeps serving.
REPO=${0:A:h:h}
BIN=$REPO/target/debug/borgd
PID_FILE=$HOME/.borg/borgd.pid
pid=$(cat $PID_FILE 2>/dev/null)
if [[ -n $pid ]] && ps -o command= -p $pid 2>/dev/null | grep -q "^$BIN"; then
  kill $pid
  for i in {1..150}; do kill -0 $pid 2>/dev/null || break; sleep 0.1; done
  if kill -0 $pid 2>/dev/null; then
    echo "[dev $(date +%T)] borgd $pid ignored SIGTERM for 15 s; sending SIGKILL"
    kill -9 $pid
  fi
fi
export BORG_DEV_KEK_CACHE=$($REPO/scripts/dev-kek-pass.sh) || exit 1
cd $HOME/.borg
# Own session: cargo watch kills the process group of its last run on the
# next change, which would SIGKILL a daemon left in it.
nohup perl -MPOSIX=setsid -e 'setsid; exec @ARGV or die "exec: $!"' $BIN >> ${BORGD_DEV_LOG:-/dev/null} 2>&1 &
disown
echo "[dev $(date +%T)] borgd started, pid $!"
