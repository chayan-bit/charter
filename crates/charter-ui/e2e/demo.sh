#!/bin/bash
# usage: demo.sh <chart-bin> <repos-cache-dir> <out-dir>
# Records the demo of `chart ui` against a fresh copy of a real repos cache, then joins the clips
# into one MP4 (needs python3 with playwright, and ffmpeg). Run it on a host that may hold the data.
set -euo pipefail
BIN=$1; CACHE=$2; OUT=$3
H=$(mktemp -d); chmod 700 "$H"; cp -a "$CACHE" "$H/repos"
rm -rf "$OUT"; mkdir -p "$OUT/clips"
PORT=$((19000 + RANDOM % 900))
CHARTER_HOME=$H timeout 300 "$BIN" ui --port "$PORT" > "$H/ui.out" 2> "$H/ui.err" &
PID=$!
trap 'kill $PID 2>/dev/null; rm -rf "$H"' EXIT
sleep 3
DEMO_URL=$(head -1 "$H/ui.out") DEMO_OUT=$OUT/clips python3 "$(dirname "$0")/demo.py"
# desktop clip first, phone clip second (recorded in that order)
ls -tr "$OUT"/clips/*.webm > "$OUT/list.txt"
d=$(sed -n 1p "$OUT/list.txt"); m=$(sed -n 2p "$OUT/list.txt")
ffmpeg -y -loglevel error -i "$d" -i "$m" -filter_complex \
  "[0:v]fps=20,scale=1280:720,setsar=1[a];[1:v]fps=20,scale=-2:720,pad=1280:720:(ow-iw)/2:0:color=#0b0b0c,setsar=1[b];[a][b]concat=n=2:v=1[v]" \
  -map "[v]" -c:v libx264 -crf 30 -preset slow -pix_fmt yuv420p -movflags +faststart "$OUT/charter-ui-demo.mp4"
ls -l "$OUT/charter-ui-demo.mp4"
