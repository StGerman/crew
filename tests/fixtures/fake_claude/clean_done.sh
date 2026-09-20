#!/bin/bash
# Two turns, then a clean successful result — the baseline "everything worked" stream.
echo '{"type":"system","subtype":"init","session_id":"test"}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"working on it"}],"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"done"}],"usage":{"input_tokens":8,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}'
echo '{"type":"result","subtype":"success","is_error":false,"num_turns":2,"result":"All done."}'
