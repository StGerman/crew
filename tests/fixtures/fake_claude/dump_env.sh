#!/bin/bash
# Dumps its own environment to the workspace (its cwd) so the test can inspect it directly,
# rather than trying to smuggle the dump through the JSON stream.
env > ./env_dump.txt
echo '{"type":"system","subtype":"init","session_id":"test"}'
echo '{"type":"result","subtype":"success","is_error":false,"num_turns":0,"result":"ok"}'
