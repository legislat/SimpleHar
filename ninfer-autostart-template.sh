#!/usr/bin/env bash
# simpleharn canonical ninfer autostart - rendered by the harness from its embedded
# template; staged content-addressed and launched INSTEAD of any operator script.
# Do not edit the staged copy: it is rewritten from the model yaml before every load.
# Profile {{PROFILE}} ({{PROFILE_SOURCE}}): ctx={{MAX_CTX}} kv={{KV_CAP}} conc={{CONC}} kv_dtype={{KV_DTYPE}}
set -euo pipefail

NAME="{{NAME}}"
IMAGE="{{IMAGE}}"
MODEL_NAME="{{MODEL_NAME}}"
ARTIFACT="{{ARTIFACT}}"
PORT="{{PORT}}"
SERVE_CMD="{{SERVE_CMD}}"
MODEL_ROOTS=(
{{MODEL_ROOTS}}
)
SERVE_ARGS=({{SERVE_ARGS}})
API_KEY_FILE="{{KEY_FILE}}"
WITH_KEY="{{KEY}}"

MODEL_DIR=""
for root in "${MODEL_ROOTS[@]}"; do
  if [[ -f "$root/$MODEL_NAME/$ARTIFACT" ]]; then MODEL_DIR="$root/$MODEL_NAME"; break; fi
done
if [[ -z "$MODEL_DIR" ]]; then
  echo "autostart: no artifact '$ARTIFACT' for model '$MODEL_NAME' under ${MODEL_ROOTS[*]}" >&2
  exit 1
fi

PUBLISH="${3:?harness must pass the publish port as arg 3}"

API_KEY_ARGS=()
if [[ "$WITH_KEY" == "key" ]]; then
  if [[ ! -s "$API_KEY_FILE" ]]; then
    echo "autostart: '$NAME' requires an API key but '$API_KEY_FILE' is missing or empty" >&2
    exit 1
  fi
  API_KEY_ARGS=(--api-key "$(cat "$API_KEY_FILE")")
fi

docker rm -f "$NAME" >/dev/null 2>&1 || true

docker run -d --name "$NAME" \
  --gpus '"device={{GPU_DEVICE}}"' \
  --restart no \
  --publish "127.0.0.1:${PUBLISH}:${PORT}" \
  --volume "$MODEL_DIR:/models:ro" \
  --log-driver json-file --log-opt max-size={{LOG_MAX_SIZE_MIB}}m --log-opt max-file={{LOG_MAX_FILES}} \
  {{ENV_ARGS}}"$IMAGE" \
  "$SERVE_CMD" "/models/$ARTIFACT" \
  --host 0.0.0.0 \
  --port "$PORT" \
  --max-context "{{MAX_CTX}}" \
  --kv-capacity "{{KV_CAP}}" \
  --max-concurrency "{{CONC}}" \
  --kv-dtype "{{KV_DTYPE}}" \
  ${API_KEY_ARGS[@]+"${API_KEY_ARGS[@]}"} \
  {{MODEL_ID_ARG}}${SERVE_ARGS[@]+"${SERVE_ARGS[@]}"}

echo "autostart: '$NAME' profile '{{PROFILE}}' (ctx={{MAX_CTX}} kv={{KV_CAP}} conc={{CONC}} kvdtype={{KV_DTYPE}} key=$([[ "$WITH_KEY" == key ]] && echo on || echo off)) on port $PUBLISH->$PORT"
