#!/usr/bin/env bash
# Start N virtual displays (:1..:N), one Chromium profile per display pointed at the app,
# record each display with ffmpeg, then run whatever command was given (default: bench).
set -euo pipefail
N="${DISPLAY_COUNT:-3}"
APP="${APP_URL:-http://app:47310}"
mkdir -p /work/recordings
for i in $(seq 1 "$N"); do
  Xvfb ":$i" -screen 0 1280x800x24 -nolisten tcp >/dev/null 2>&1 &
  sleep 0.3
  DISPLAY=":$i" chromium --no-sandbox --disable-gpu --disable-dev-shm-usage \
    --user-data-dir="/tmp/chrome-lane-$i" --window-size=1280,800 --window-position=0,0 \
    "$APP" >/dev/null 2>&1 &
  ffmpeg -loglevel error -y -f x11grab -video_size 1280x800 -framerate 8 -i ":$i" \
    -c:v libx264 -preset veryfast -pix_fmt yuv420p "/work/recordings/lane-$i.mp4" >/dev/null 2>&1 &
done
echo "lanes: $N displays up, recording to /work/recordings"
if [ "$#" -eq 0 ]; then
  exec bench run --app "$APP" --arms "${ARMS:-D,E}" --runs "${RUNS:-5}" --phase "${PHASE:-3}" --out /work/results/sandbox
else
  exec "$@"
fi
