#!/bin/bash
# A successful result whose final text carries the SYMPHONY_OUTCOME continue marker.
echo '{"type":"system","subtype":"init","session_id":"test"}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"partway there"}],"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}'
echo '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"Made progress.\nSYMPHONY_OUTCOME: continue: need another turn to finish tests"}'
