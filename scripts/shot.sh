#!/bin/sh
# Screenshot only the app's window region, never the rest of the screen.
# usage: scripts/shot.sh OUT.png [app args...]   (env: TW_WINDOW=x,y,w,h  TW_SETTLE=seconds)
set -eu
out=$1; shift
bounds=${TW_WINDOW:-80,80,1440,900}
export TW_WINDOW="$bounds"
bin="$(dirname "$0")/../target/release/trace-waterfall"
"$bin" "$@" >/tmp/tw-shot.log 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null || true' EXIT
sleep "${TW_SETTLE:-2.5}"
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $pid) to true" >/dev/null 2>&1 || true
sleep 0.6
screencapture -x -o -R "$bounds" "$out"
