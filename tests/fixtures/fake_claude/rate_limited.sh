#!/bin/bash
# One turn, then a rejected account-wide rate limit, then the process dies without a result
# event — the real CLI's shape when a session limit is hit (#37).
echo '{"type":"system","subtype":"init","session_id":"test"}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"working on it"}],"usage":{"input_tokens":5,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}'
echo '{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour","resetsAt":1789981200,"overageDisabledReason":"out_of_credits","unifiedWindows":{"five_hour":{"utilization":1.05,"resetsAt":1789981200},"seven_day":{"utilization":0.37,"resetsAt":1790438400}}}}'
exit 1
