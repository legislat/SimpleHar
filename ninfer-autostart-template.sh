#!/usr/bin/env bash
# autostart.sh вЂ” start one NInfer service with a VRAM profile: a|b|c|d (default b)
set -euo pipefail

NAME="SE-nin-mymodel"                 # == ninfer_container in the model yaml
IMAGE="ninfer-mymodel:local"          # the image your artifact requires
MODEL_NAME="My-Model-NInfer"          # directory name holding the artifact
ARTIFACT="my-model.ninfer"            # file name inside the model directory
MODEL_ID="my-model"                   # OpenAI-style served id (== serve_model in yaml)
INTERNAL_PORT=8005                    # port the server listens on inside the container

# Model directories to search for ARTIFACT (first hit wins).
MODEL_ROOTS=(
  "/mnt/d/Models"
  "/home/you/ai/models"
)
MODEL_DIR=""
for root in "${MODEL_ROOTS[@]}"; do
  if [[ -f "$root/$MODEL_NAME/$ARTIFACT" ]]; then MODEL_DIR="$root/$MODEL_NAME"; break; fi
done
[[ -n "$MODEL_DIR" ]] || { echo "autostart: no '$ARTIFACT' under ${MODEL_ROOTS[*]}" >&2; exit 1; }

CFG="${1:-b}"
WITH_KEY="${2:-}"
case "$CFG" in
  a) MAX_CTX=262144; KV_CAP=524288; CONC=2 ;;  # big context, 2 concurrent
  b) MAX_CTX=262144; KV_CAP=262144; CONC=1 ;;  # default: large single agent context
  c) MAX_CTX=131072; KV_CAP=131072; CONC=1 ;;  # most VRAM-conserving
  d) MAX_CTX=16384;  KV_CAP=32768;  CONC=2 ;;  # short context, 2 concurrent
  *) echo "usage: $0 <a|b|c|d> [key]" >&2; exit 1 ;;
esac

if [[ "$WITH_KEY" == "key" ]]; then
  API_KEY="$(cat "$(dirname "$0")/.api_key")"  # keep the secret in a file, never here
  API_KEY_ARGS=(--api-key "$API_KEY")
else
  API_KEY_ARGS=()
fi

PUBLISH="${3:-$INTERNAL_PORT}"        # host port assigned by the harness

# Idempotent: reclaim a stale container of OUR name before starting.
docker rm -f "$NAME" >/dev/null 2>&1 || true

docker run -d --name "$NAME" \
  --gpus '"device=0"' \
  --restart unless-stopped \
  --publish "127.0.0.1:${PUBLISH}:${INTERNAL_PORT}" \
  --volume "$MODEL_DIR:/models:ro" \
  "$IMAGE" \
  serve "/models/$ARTIFACT" \
  --host 0.0.0.0 \
  --port "$INTERNAL_PORT" \
  --model-id "$MODEL_ID" \
  "${API_KEY_ARGS[@]}" \
  --max-context "$MAX_CTX" \
  --kv-capacity "$KV_CAP" \
  --max-concurrency "$CONC"

echo "autostart: '$NAME' profile '$CFG' (ctx=$MAX_CTX kv=$KV_CAP conc=$CONC key=$([[ "$WITH_KEY" == "key" ]] && echo on || echo off)) on port $PUBLISH->$INTERNAL_PORT"
