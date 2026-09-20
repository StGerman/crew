#!/bin/bash
# A good line, then a truncated one with no closing brace or trailing newline — simulates a
# process killed or crashing mid-write.
echo '{"type":"system","subtype":"init","session_id":"test"}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"partial next"}],"usage":{"input_tokens":5,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}'
printf '{"type":"resul'
