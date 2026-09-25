#!/bin/bash
# What `claude 2.1.282` emits for `--model definitely-not-a-model`, trimmed to the fields read:
# a synthetic turn naming the error, then an error `result` — and exit 0, not a refusal.
echo '{"type":"system","subtype":"init","session_id":"test"}'
echo '{"type":"assistant","message":{"model":"<synthetic>","content":[{"type":"text","text":"There'"'"'s an issue with the selected model (definitely-not-a-model). It may not exist or you may not have access to it."}]},"error":"model_not_found","is_api_error_message":true}'
echo '{"type":"result","subtype":"success","is_error":true,"api_error_status":404,"num_turns":1,"result":"There'"'"'s an issue with the selected model (definitely-not-a-model). It may not exist or you may not have access to it."}'
