#!/bin/sh
# Print what VoiceOver reads: the app's accessibility tree as role, label and value, one element per line.
# usage: scripts/ax.sh [app args...]   (env: TW_WINDOW=x,y,w,h  TW_SETTLE=seconds)
set -eu
export TW_WINDOW="${TW_WINDOW:-80,80,1440,900}"
"$(dirname "$0")/../target/release/trace-waterfall" "$@" >/tmp/tw-ax.log 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null || true' EXIT
sleep "${TW_SETTLE:-3}"
osascript - "$pid" <<'EOF'
on walk(el, depth, out)
	if depth > 6 then return out
	tell application "System Events"
		set r to ""
		try
			set r to role of el
		end try
		set d to ""
		try
			set d to value of attribute "AXDescription" of el
		end try
		try
			if d is "" or d is missing value then set d to value of attribute "AXTitle" of el
		end try
		set v to ""
		try
			set v to value of attribute "AXValue" of el as text
		end try
		set pad to ""
		repeat depth times
			set pad to pad & "  "
		end repeat
		set out to out & pad & r & " | " & d & " | " & v & linefeed
		set kids to {}
		try
			set kids to UI elements of el
		end try
	end tell
	repeat with k in kids
		set out to my walk(k, depth + 1, out)
	end repeat
	return out
end walk

on run argv
	tell application "System Events" to set w to window 1 of (first process whose unix id is (item 1 of argv as integer))
	-- The first query wakes the tree; AccessKit builds it only once an assistive client asks.
	tell application "System Events" to get UI elements of w
	delay 1
	return my walk(w, 0, "")
end run
EOF
