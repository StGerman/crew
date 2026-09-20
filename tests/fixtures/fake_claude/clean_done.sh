#!/bin/bash
# Two turns with a tool result between them, then a clean successful result — the baseline
# "everything worked" stream. The result's usage deliberately disagrees with the sum of the
# assistant events' usage (312/60 vs 18/8): the result is authoritative and a reader that sums
# the events instead must fail the test that checks this.
echo '{"type":"system","subtype":"init","session_id":"test"}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"working on it"}],"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}'
echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"done"}],"usage":{"input_tokens":8,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}'
echo '{"type":"result","subtype":"success","is_error":false,"num_turns":2,"result":"All done.","usage":{"input_tokens":12,"cache_creation_input_tokens":100,"cache_read_input_tokens":200,"output_tokens":60}}'
