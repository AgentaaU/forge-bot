#!/usr/bin/env python3
"""Minimal `kimi --wire` stand-in for the kimi_wire tests.

Wire is JSON-RPC 2.0 over newline-delimited JSON. Notifications use the
``event`` method with a ``{type, payload}`` body; server requests use the
``request`` method and expect a response.

Environment:

* ``FAKE_KIMI_LOG`` writes every request line as JSON.
* ``FAKE_KIMI_WAIT_FOR_STEER=1`` holds the prompt open until ``steer``.
* ``FAKE_KIMI_STEER_LOG`` records steer text.
* ``FAKE_KIMI_APPROVAL=1`` emits an ``ApprovalRequest`` before finishing and
  waits for the client's response (recorded in ``FAKE_KIMI_APPROVAL_LOG``).
* ``FAKE_KIMI_STATUS`` overrides the final prompt status (``cancelled``).
* ``FAKE_KIMI_REJECT_STEER=1`` answers ``steer`` with an error.
* ``FAKE_KIMI_PROMPT_LOG`` records the submitted prompt.
* ``FAKE_KIMI_EXIT_ON_PROMPT=1`` records the prompt and exits without answering.
* ``FAKE_KIMI_STEER_DELAY`` delays the ``steer`` acknowledgment.
* ``FAKE_KIMI_STEER_EXIT=1`` records the steer and exits without answering it.
"""

import json
import os
import sys
import time

log_path = os.environ.get("FAKE_KIMI_LOG")
wait_for_steer = os.environ.get("FAKE_KIMI_WAIT_FOR_STEER") == "1"
steer_log = os.environ.get("FAKE_KIMI_STEER_LOG")
approval = os.environ.get("FAKE_KIMI_APPROVAL") == "1"
approval_log = os.environ.get("FAKE_KIMI_APPROVAL_LOG")
status = os.environ.get("FAKE_KIMI_STATUS", "finished")
reject_steer = os.environ.get("FAKE_KIMI_REJECT_STEER") == "1"
delay = float(os.environ.get("FAKE_KIMI_DELAY", "0"))
prompt_log = os.environ.get("FAKE_KIMI_PROMPT_LOG")
exit_on_prompt = os.environ.get("FAKE_KIMI_EXIT_ON_PROMPT") == "1"
steer_delay = float(os.environ.get("FAKE_KIMI_STEER_DELAY", "0"))
steer_exit = os.environ.get("FAKE_KIMI_STEER_EXIT") == "1"

prompt_id = None
prompt_open = False
steered = []
waiting_approval = False


def send(message):
    print(json.dumps(message), flush=True)


def finish():
    global prompt_open
    text = "KIMI-REPLY"
    if steered:
        text = "steered: " + " ".join(steered)
    send({"jsonrpc": "2.0", "method": "event",
          "params": {"type": "ContentPart", "payload": {"type": "text", "text": text}}})
    send({"jsonrpc": "2.0", "method": "event", "params": {"type": "TurnEnd", "payload": {}}})
    send({"jsonrpc": "2.0", "id": prompt_id, "result": {"status": status}})
    prompt_open = False


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    if log_path:
        with open(log_path, "a") as handle:
            handle.write(json.dumps(json.loads(line)) + "\n")
    try:
        message = json.loads(line)
    except ValueError:
        continue
    method = message.get("method")
    request_id = message.get("id")
    params = message.get("params") or {}

    if method is None:
        # A response to one of our server requests.
        if waiting_approval and request_id == "srv-approval":
            waiting_approval = False
            if approval_log:
                with open(approval_log, "w") as handle:
                    handle.write(json.dumps(message.get("result")))
            finish()
        continue

    if method == "initialize":
        send({"jsonrpc": "2.0", "id": request_id,
              "result": {"protocol_version": "1.4", "server": {"name": "Kimi", "version": "fake"},
                         "slash_commands": []}})
    elif method == "prompt":
        prompt_id = request_id
        if prompt_log:
            with open(prompt_log, "a") as handle:
                handle.write("prompt\n")
        if exit_on_prompt:
            sys.exit(0)
        time.sleep(delay)
        send({"jsonrpc": "2.0", "method": "event",
              "params": {"type": "TurnBegin", "payload": {"user_input": params.get("user_input", "")}}})
        send({"jsonrpc": "2.0", "method": "event",
              "params": {"type": "StatusUpdate",
                         "payload": {"token_usage": {"input_other": 700, "output": 10,
                                                     "input_cache_read": 250,
                                                     "input_cache_creation": 50}}}})
        if approval:
            waiting_approval = True
            send({"jsonrpc": "2.0", "method": "request", "id": "srv-approval",
                  "params": {"type": "ApprovalRequest",
                             "payload": {"id": "approval-1", "tool_call_id": "tc-1",
                                         "sender": "Shell", "action": "run shell command",
                                         "description": "Run `ls`", "display": []}}})
            continue
        if wait_for_steer:
            prompt_open = True
            continue
        finish()
    elif method == "steer":
        if reject_steer:
            send({"jsonrpc": "2.0", "id": request_id,
                  "error": {"code": -32000, "message": "No agent turn is in progress"}})
            if prompt_open:
                finish()
            continue
        text = params.get("user_input", "")
        steered.append(text)
        if steer_log:
            with open(steer_log, "a") as handle:
                handle.write(text + "\n")
        if steer_exit:
            sys.exit(0)
        if steer_delay:
            time.sleep(steer_delay)
        send({"jsonrpc": "2.0", "id": request_id, "result": {"status": "steered"}})
        send({"jsonrpc": "2.0", "method": "event",
              "params": {"type": "SteerInput", "payload": {"user_input": text}}})
        if prompt_open:
            finish()
    elif method == "cancel":
        send({"jsonrpc": "2.0", "id": request_id, "result": {}})
        if prompt_open:
            send({"jsonrpc": "2.0", "id": prompt_id, "result": {"status": "cancelled"}})
            prompt_open = False
