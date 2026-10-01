#!/usr/bin/env python3
"""Minimal `codex app-server` stand-in for the codex_app_server tests.

The protocol is newline-delimited JSON (no JSON-RPC envelope): a request has
``id``/``method``/``params`` and the response carries ``id``/``result``.
Notifications carry ``method``/``params`` with no ``id``.

Environment:

* ``FAKE_CODEX_LOG`` writes every request line as JSON.
* ``FAKE_CODEX_WAIT_FOR_STEER=1`` holds the turn open after its first delta
  until a ``turn/steer`` arrives.
* ``FAKE_CODEX_STEER_LOG`` records steer text.
* ``FAKE_CODEX_STATUS`` overrides the final turn status (``failed``).
* ``FAKE_CODEX_REJECT_STEER=1`` answers ``turn/steer`` with an error.
* ``FAKE_CODEX_FAIL_TURN=1`` answers ``turn/start`` with an error.
* ``FAKE_CODEX_APPROVAL=1`` emits an approval request and waits for its answer.
* ``FAKE_CODEX_EXIT_AFTER_TURN_START=1`` acknowledges ``turn/start`` and exits
  without emitting ``turn/completed``.
* ``FAKE_CODEX_STEER_DELAY`` delays the ``turn/steer`` acknowledgment.
* ``FAKE_CODEX_STEER_EXIT=1`` records the steer and exits without answering it.
"""

import json
import os
import sys
import time

log_path = os.environ.get("FAKE_CODEX_LOG")
wait_for_steer = os.environ.get("FAKE_CODEX_WAIT_FOR_STEER") == "1"
steer_log = os.environ.get("FAKE_CODEX_STEER_LOG")
status = os.environ.get("FAKE_CODEX_STATUS", "completed")
reject_steer = os.environ.get("FAKE_CODEX_REJECT_STEER") == "1"
fail_turn = os.environ.get("FAKE_CODEX_FAIL_TURN") == "1"
approval = os.environ.get("FAKE_CODEX_APPROVAL") == "1"
delay = float(os.environ.get("FAKE_CODEX_DELAY", "0"))
exit_after_turn_start = os.environ.get("FAKE_CODEX_EXIT_AFTER_TURN_START") == "1"
steer_delay = float(os.environ.get("FAKE_CODEX_STEER_DELAY", "0"))
steer_exit = os.environ.get("FAKE_CODEX_STEER_EXIT") == "1"

turn_open = False
steered = []
waiting_approval = False


def send(message):
    print(json.dumps(message), flush=True)


def respond(request_id, result):
    send({"id": request_id, "result": result})


def finish_turn(turn_status=None):
    final = turn_status or status
    text = "CODEX-REPLY"
    if steered:
        text = "steered: " + " ".join(steered)
    send({"method": "item/agentMessage/delta",
          "params": {"threadId": "thread-1", "turnId": "turn-1", "itemId": "i1",
                     "delta": "REPLY"}})
    send({"method": "thread/tokenUsage/updated",
          "params": {"threadId": "thread-1", "turnId": "turn-1",
                     "tokenUsage": {"last": {"inputTokens": 1000, "cachedInputTokens": 750,
                                             "outputTokens": 5, "reasoningOutputTokens": 0,
                                             "totalTokens": 1005},
                                    "total": {"inputTokens": 1000, "cachedInputTokens": 750,
                                              "outputTokens": 5, "reasoningOutputTokens": 0,
                                              "totalTokens": 1005}}}})
    turn = {"id": "turn-1", "status": final, "items": [
        {"type": "agentMessage", "id": "i1", "text": text}]}
    if final == "failed":
        turn["error"] = {"message": "the model refused the request"}
    send({"method": "turn/completed",
          "params": {"threadId": "thread-1", "turn": turn}})


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
        if waiting_approval and request_id == "srv-approval":
            waiting_approval = False
            finish_turn()
        continue

    if method == "initialize":
        respond(request_id, {"codexHome": "/tmp/codex", "platformFamily": "unix",
                             "platformOs": "linux", "userAgent": "fake"})
    elif method == "thread/start":
        respond(request_id, {"thread": {"id": "thread-1"}})
    elif method == "thread/resume":
        if params.get("threadId") == "thread-1":
            respond(request_id, {"thread": {"id": "thread-1"}})
        else:
            send({"id": request_id, "error": {"code": -32600, "message": "unknown thread"}})
    elif method == "turn/start":
        if fail_turn:
            send({"id": request_id, "error": {"code": -32600, "message": "turn start failed"}})
            continue
        send({"method": "turn/started",
              "params": {"threadId": "thread-1", "turn": {"id": "turn-1"}}})
        respond(request_id, {"turn": {"id": "turn-1", "status": "inProgress", "items": []}})
        if exit_after_turn_start:
            sys.exit(0)
        time.sleep(delay)
        send({"method": "item/agentMessage/delta",
              "params": {"threadId": "thread-1", "turnId": "turn-1", "itemId": "i1",
                         "delta": "CODEX-"}})
        if approval:
            waiting_approval = True
            send({"method": "item/commandExecution/requestApproval", "id": "srv-approval",
                  "params": {"threadId": "thread-1", "turnId": "turn-1", "itemId": "i1"}})
            continue
        if wait_for_steer:
            turn_open = True
        else:
            finish_turn()
    elif method == "turn/steer":
        if reject_steer:
            send({"id": request_id, "error": {"code": -32600, "message": "no active turn"}})
            finish_turn()
            continue
        text = ""
        for part in params.get("input", []):
            if part.get("type") == "text":
                text += part.get("text", "")
        if steer_log:
            with open(steer_log, "a") as handle:
                handle.write(text + "\n")
        steered.append(text)
        if steer_exit:
            sys.exit(0)
        if steer_delay:
            time.sleep(steer_delay)
        respond(request_id, {"turnId": "turn-1"})
        if turn_open:
            turn_open = False
            finish_turn()
    elif method == "turn/interrupt":
        respond(request_id, {})
        finish_turn("interrupted")
    elif method == "shutdown":
        respond(request_id, {})
