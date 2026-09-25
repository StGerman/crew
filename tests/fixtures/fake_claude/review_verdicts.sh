#!/bin/bash
# A successful result whose final text settles two review comments and leaves a third alone,
# with one malformed line the parser must skip rather than choke on, and one acceptance that
# names no commit — a bare acknowledgement the parser must not take for a fix.
echo '{"type":"system","subtype":"init","session_id":"test"}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"fixed one, declined one"}],"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}'
echo '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"Done with the review.\nCREW_REVIEW: 4059939692: accepted: a1b2c3d\nCREW_REVIEW: 4059939693: rejected: the umask concern is handled by restrict() two lines below\nCREW_REVIEW: 4059939694: shrug\nCREW_REVIEW: 4059939695: accepted: fixed\n"}'
