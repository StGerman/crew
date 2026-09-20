#!/bin/bash
# Dumps the argv it was invoked with to the workspace (its cwd), so a test can assert on the
# command line the worker actually builds rather than on a restatement of it.
printf '%s\n' "$@" > ./argv_dump.txt
echo '{"type":"system","subtype":"init","session_id":"test"}'
echo '{"type":"result","subtype":"success","is_error":false,"num_turns":0,"result":"ok"}'
