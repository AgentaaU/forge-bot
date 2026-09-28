#!/usr/bin/env python3
"""Minimal pi RPC stand-in for the pi_rpc pool tests.

Set ``FAKE_PI_WAIT_FOR_STEER=1`` to hold the run open after a prompt until a
``steer``/``follow_up`` command arrives. ``FAKE_PI_STEER_LOG`` records injected
follow-ups and ``FAKE_PI_RESULT`` lets the follow-up text become the final
assistant message.
"""

import json
import os
import sys
import time

delay = float(os.environ.get("FAKE_PI_DELAY", "0"))
wait_for_steer = os.environ.get("FAKE_PI_WAIT_FOR_STEER") == "1"
steer_log = os.environ.get("FAKE_PI_STEER_LOG")
result_path = os.environ.get("FAKE_PI_RESULT")
stream_text = os.environ.get("FAKE_PI_STREAM_TEXT")
prompt_log = os.environ.get("FAKE_PI_PROMPT_LOG")

waiting = False
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        message = json.loads(line)
    except ValueError:
        continue
    kind = message.get("type")
    request_id = message.get("id")
    if kind == "prompt":
        if prompt_log:
            with open(prompt_log, "w") as handle:
                handle.write("received")
        time.sleep(delay)
        print(
            json.dumps({"type": "response", "id": request_id, "success": True}),
            flush=True,
        )
        if stream_text:
            print(json.dumps({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": stream_text}}), flush=True)
        if wait_for_steer:
            waiting = True
        else:
            print(json.dumps({"type": "agent_settled"}), flush=True)
    elif kind in ("steer", "follow_up"):
        text = message.get("message", "")
        if steer_log:
            with open(steer_log, "a") as handle:
                handle.write(f"{kind}:{text}\n")
        if result_path:
            with open(result_path, "w") as handle:
                handle.write(text)
        print(
            json.dumps({"type": "response", "command": kind, "success": True}),
            flush=True,
        )
        if waiting:
            waiting = False
            print(json.dumps({"type": "agent_settled"}), flush=True)
    elif kind == "get_last_assistant_text":
        text = "fake-result"
        if result_path and os.path.exists(result_path):
            with open(result_path) as handle:
                text = handle.read()
        print(
            json.dumps(
                {
                    "type": "response",
                    "id": request_id,
                    "data": {"text": text},
                }
            ),
            flush=True,
        )
    elif kind == "get_state":
        print(json.dumps({"type": "response", "id": request_id, "success": True,
                          "data": {"model": {"provider": "test", "id": "fake-pi"}}}), flush=True)
