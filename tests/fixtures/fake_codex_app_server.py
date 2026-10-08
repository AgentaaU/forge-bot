#!/usr/bin/env python3
"""Minimal `codex app-server` stand-in for the codex_app_server tests.

The protocol is newline-delimited JSON (no JSON-RPC envelope): a request has
``id``/``method``/``params`` and the response carries ``id``/``result``.
Notifications carry ``method``/``params`` with no ``id``.

Environment:

* ``FAKE_CODEX_LOG`` writes every request line as JSON.
* ``FAKE_CODEX_OMIT_MODEL=1`` omits model metadata from thread responses.
* ``FAKE_CODEX_RESUME_MODEL`` overrides the model reported on resume.
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
* ``FAKE_CODEX_PID_FILE`` records this process's pid so tests can check it stops.
* ``FAKE_CODEX_CONFIG_EFFORT`` is the configured ``model_reasoning_effort``
  (returned by ``config/read``); unset means none is configured.
* ``FAKE_CODEX_MODEL_EFFORT`` is the default effort of ``codex-default``
  (``model/list``), used when none is configured. Defaults to ``medium``.
* ``FAKE_CODEX_STATE`` persists the thread's effort override across processes.
  Like Codex, a ``turn/start`` with an effort sets the override, while one
  without it (or with null) keeps the previous one.
* ``FAKE_CODEX_EFFECTIVE_LOG`` appends the effective ``reasoningEffort`` each
  ``thread/resume`` reports.
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
pid_file = os.environ.get("FAKE_CODEX_PID_FILE")
config_effort = os.environ.get("FAKE_CODEX_CONFIG_EFFORT") or None
model_effort = os.environ.get("FAKE_CODEX_MODEL_EFFORT", "medium")
state_path = os.environ.get("FAKE_CODEX_STATE")
effective_log = os.environ.get("FAKE_CODEX_EFFECTIVE_LOG")
if pid_file:
    with open(pid_file, "w") as handle:
        handle.write(str(os.getpid()))

turn_open = False
steered = []
waiting_approval = False


def send(message):
    print(json.dumps(message), flush=True)


def respond(request_id, result):
    send({"id": request_id, "result": result})


def load_override():
    if state_path and os.path.exists(state_path):
        with open(state_path) as handle:
            return json.load(handle).get("effort")
    return None


def effective_effort():
    return load_override() or config_effort or model_effort


def respond_thread(request_id, params, resume=False):
    result = {"thread": {"id": "thread-1"}, "reasoningEffort": effective_effort()}
    if resume and effective_log:
        with open(effective_log, "a") as handle:
            handle.write(result["reasoningEffort"] + "\n")
    if os.environ.get("FAKE_CODEX_OMIT_MODEL") != "1":
        model = params.get("model", "codex-default")
        if resume:
            model = os.environ.get("FAKE_CODEX_RESUME_MODEL", model)
        result["model"] = model
    respond(request_id, result)


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
    elif method == "config/read":
        respond(request_id, {"config": {"model_reasoning_effort": config_effort},
                             "origins": {}, "layers": None})
    elif method == "model/list":
        respond(request_id, {"data": [{"model": "codex-default",
                                       "defaultReasoningEffort": model_effort}],
                             "nextCursor": None})
    elif method == "thread/start":
        respond_thread(request_id, params)
    elif method == "thread/resume":
        if params.get("threadId") == "thread-1":
            respond_thread(request_id, params, resume=True)
        else:
            send({"id": request_id, "error": {"code": -32600, "message": "unknown thread"}})
    elif method == "turn/start":
        if fail_turn:
            send({"id": request_id, "error": {"code": -32600, "message": "turn start failed"}})
            continue
        if state_path and isinstance(params.get("effort"), str):
            with open(state_path, "w") as handle:
                json.dump({"effort": params["effort"]}, handle)
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
