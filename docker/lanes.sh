#!/usr/bin/env bash
# Run the bench inside the container. Every screen arm is headless CDP now, so there is no
# virtual display to start and nothing that could ever touch a host screen. Arm A saves the
# screenshots it acted on under results/<run>/shots/.
set -euo pipefail
APP="${APP_URL:-http://app:47310}"
export CHIFFON_CHROME="${CHIFFON_CHROME:-/usr/bin/chromium}"
if [ "$#" -eq 0 ]; then
  exec bench run --app "$APP" --arms "${ARMS:-D,E}" --runs "${RUNS:-5}" --phase "${PHASE:-3}" \
    ${PLANNER:+--planner "$PLANNER"} ${MODEL:+--model "$MODEL"} ${BASE_URL:+--base-url "$BASE_URL"} \
    --out "/work/results/${OUT:-sandbox}"
else
  exec "$@"
fi
