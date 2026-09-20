#!/bin/bash
# Writes its own pid for the test to check against after kill, ignores SIGTERM, then hangs —
# the wedged-process case that forces an escalation to SIGKILL.
trap '' TERM
echo $$ > ./pid.txt
echo '{"type":"system","subtype":"init","session_id":"test"}'
while true; do sleep 1; done
