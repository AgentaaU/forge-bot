#!/usr/bin/env python3
"""Minimal pi RPC stand-in for the pi_rpc pool tests.

Set ``FAKE_PI_WAIT_FOR_STEER=1`` to hold the run open after a prompt until a
``steer``/``follow_up`` command arrives. ``FAKE_PI_STEER_LOG`` records injected
follow-ups and ``FAKE_PI_RESULT`` lets the follow-up text become the final
assistant message.

``FAKE_PI_COMMAND_LOG`` records ``new_session``/``switch_session`` calls, and
``FAKE_PI_SESSION_LOG`` records the session id used for each prompt, so the pool
tests can assert on session management. ``FAKE_PI_CANCEL_ONCE`` is a
comma-separated list of session commands that fail once with
``data.cancelled = true`` before succeeding, to exercise the error paths.
"""

import json
import os
import sys
import time
import uuid

delay = float(os.environ.get("FAKE_PI_DELAY", "0"))
wait_for_steer = os.environ.get("FAKE_PI_WAIT_FOR_STEER") == "1"
steer_log = os.environ.get("FAKE_PI_STEER_LOG")
result_path = os.environ.get("FAKE_PI_RESULT")
stream_text = os.environ.get("FAKE_PI_STREAM_TEXT")
prompt_log = os.environ.get("FAKE_PI_PROMPT_LOG")
command_log = os.environ.get("FAKE_PI_COMMAND_LOG")
session_log = os.environ.get("FAKE_PI_SESSION_LOG")

argv = sys.argv[1:]
session_id = os.environ.get("FAKE_PI_SESSION_ID")
if not session_id:
    if "--session-id" in argv:
        session_id = argv[argv.index("--session-id") + 1]
    else:
        session_id = "11111111-1111-5111-8111-111111111111"
session_file = os.environ.get(
    "FAKE_PI_SESSION_FILE", f"/tmp/fake-pi-{session_id}.jsonl"
)
cancel_once = {name for name in os.environ.get("FAKE_PI_CANCEL_ONCE", "").split(",") if name}
canceled = set()


def log_command(name, detail=""):
    """Record a session-management command for the pool tests to assert on."""
    if command_log:
        with open(command_log, "a") as handle:
            handle.write(name + (" " + detail if detail else "") + "\n")


def log_session():
    """Record the session a prompt runs in, to prove what a retry resumes."""
    if session_log:
        with open(session_log, "a") as handle:
            handle.write(session_id + "\n")


def emit_usage():
    """Report one assistant message's prompt-cache accounting."""
    print(
        json.dumps(
            {
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "usage": {"input": 100, "cacheRead": 900, "output": 10},
                },
            }
        ),
        flush=True,
    )


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
        log_session()
        time.sleep(delay)
        print(
            json.dumps({"type": "response", "id": request_id, "success": True}),
            flush=True,
        )
        if stream_text:
            print(json.dumps({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": stream_text}}), flush=True)
        if os.environ.get("FAKE_PI_EVENTS"):
            for event in json.loads(os.environ["FAKE_PI_EVENTS"]):
                print(json.dumps(event), flush=True)
        elif wait_for_steer:
            waiting = True
        else:
            emit_usage()
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
            emit_usage()
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
                          "data": {"model": {"provider": "test", "id": "fake-pi"},
                                   "sessionId": session_id,
                                   "sessionFile": session_file}}), flush=True)
    elif kind == "new_session":
        if "new_session" in cancel_once and "new_session" not in canceled:
            canceled.add("new_session")
            log_command("new_session canceled")
            print(json.dumps({"type": "response", "id": request_id,
                              "command": "new_session", "success": True,
                              "data": {"cancelled": True}}), flush=True)
            continue
        # Start a fresh session in the already-running process. The id and file
        # change so a test can tell that the pool reset the conversation.
        session_id = str(uuid.uuid4())
        session_file = f"/tmp/fake-pi-{session_id}.jsonl"
        log_command("new_session")
        print(json.dumps({"type": "response", "id": request_id,
                          "command": "new_session", "success": True,
                          "data": {"cancelled": False}}), flush=True)
    elif kind == "switch_session":
        path = message.get("sessionPath", "")
        if "switch_session" in cancel_once and "switch_session" not in canceled:
            canceled.add("switch_session")
            log_command("switch_session canceled", path)
            print(json.dumps({"type": "response", "id": request_id,
                              "command": "switch_session", "success": True,
                              "data": {"cancelled": True}}), flush=True)
            continue
        # Recover the id the pool switched to, so the next get_state reports it.
        restored = os.path.basename(path).rsplit("_", 1)[-1]
        if restored.endswith(".jsonl"):
            restored = restored[: -len(".jsonl")]
        if restored:
            session_id = restored
            session_file = path
        log_command("switch_session", path)
        print(json.dumps({"type": "response", "id": request_id,
                          "command": "switch_session", "success": True,
                          "data": {"cancelled": False}}), flush=True)
