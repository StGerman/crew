#!/bin/bash
# A stream with the lines the parser drops: `system`, `rate_limit_event`, a tool call and its
# result. None of them reach Progress or Outcome, and all of them are what a post-mortem reads.
echo '{"type":"system","subtype":"init","session_id":"test","tools":["Bash","Edit"]}'
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}],"usage":{"input_tokens":10,"output_tokens":5}}}'
echo '{"type":"user","message":{"content":[{"type":"tool_result","content":"114 passed"}]}}'
echo '{"type":"rate_limit_event","retry_after":30}'
echo 'this line is not JSON at all'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"done"}],"usage":{"input_tokens":8,"output_tokens":3}}}'
echo '{"type":"result","subtype":"success","is_error":false,"num_turns":2,"result":"All done."}'
