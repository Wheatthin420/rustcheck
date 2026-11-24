#!/usr/bin/env bash
# rustwatch — background "always-on" editor check: when any .rs file under DIR changes,
# run `rustcheck save` there (fmt in place + only-new diagnostics) and append to a log.
# Polling-based (no inotify-tools dependency), ~0.5 s detect + 0.4 s debounce.
#
# usage: rustwatch start [DIR] | rustwatch stop [DIR] | rustwatch status [DIR] | rustwatch log [DIR]
# DIR defaults to $PWD. One watcher per DIR (pid/log keyed by path hash).
set -u

CMD="${1:-status}"; DIR="${2:-$PWD}"
DIR="$(cd "$DIR" 2>/dev/null && pwd)" || { echo "rustwatch: bad dir"; exit 2; }
STATE_DIR="$HOME/.local/state/rustwatch"; mkdir -p "$STATE_DIR"
HASH="$(printf '%s' "$DIR" | md5sum | cut -c1-10)"
PIDF="$STATE_DIR/$HASH.pid"; LOGF="$STATE_DIR/$HASH.log"; MARKF="$STATE_DIR/$HASH.stamp"

alive() { [ -f "$PIDF" ] && kill -0 "$(cat "$PIDF")" 2>/dev/null; }

loop() {
  echo $$ > "$PIDF"
  touch "$MARKF"
  while :; do
    sleep 0.5
    if find "$DIR" -name '*.rs' -not -path '*/target/*' -newer "$MARKF" | grep -q .; then
      touch "$MARKF"; sleep 0.4
      echo "── $(date '+%H:%M:%S') change detected ──" >> "$LOGF"
      ( cd "$DIR" && rustcheck save ) >> "$LOGF" 2>&1
    fi
  done
}

case "$CMD" in
  start)
    [ -f "$DIR/Cargo.toml" ] || { echo "rustwatch: no Cargo.toml in $DIR"; exit 2; }
    if alive; then echo "rustwatch: already running (pid $(cat "$PIDF")) for $DIR"; exit 0; fi
    setsid bash "$0" _loop "$DIR" </dev/null >/dev/null 2>&1 &
    sleep 0.3
    if alive; then echo "rustwatch: watching $DIR -> log $LOGF"; else echo "rustwatch: failed to start"; exit 2; fi
    ;;
  _loop) loop ;;
  stop)
    if alive; then kill "$(cat "$PIDF")" && rm -f "$PIDF" && echo "rustwatch: stopped"; else echo "rustwatch: not running"; fi
    ;;
  status)
    if alive; then echo "rustwatch: running pid $(cat "$PIDF") for $DIR"; else echo "rustwatch: not running for $DIR"; fi
    tail -n 6 "$LOGF" 2>/dev/null
    ;;
  log) tail -n "${3:-30}" "$LOGF" 2>/dev/null ;;
  *) echo "usage: rustwatch start|stop|status|log [DIR]"; exit 2 ;;
esac
