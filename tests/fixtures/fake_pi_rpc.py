#!/usr/bin/env python3
"""Minimal pi RPC stand-in for the pi_rpc pool tests.

Set ``FAKE_PI_WAIT_FOR_STEER=1`` to hold the run open after a prompt until a
``steer``/``follow_up`` command arrives. ``FAKE_PI_STEER_LOG`` records injected
follow-ups and ``FAKE_PI_RESULT`` lets the follow-up text become the final
assistant message. ``FAKE_PI_COMMAND_LOG`` records ``new_session`` and
``switch_session`` calls so the pool tests can assert on session management.
Set ``FAKE_PI_CANCEL_NEW_SESSION=1`` or ``FAKE_PI_CANCEL_SWITCH_SESSION=1``
to answer that command with ``{success: false, data: {cancelled: true}}``, the
way a real pi reports a cancelled session operation. Set
``FAKE_PI_HANG_NEW_SESSION=1`` to accept ``new_session`` without ever
answering, so a test can interrupt a run while preparation is in flight.
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
cancel_new_session = os.environ.get("FAKE_PI_CANCEL_NEW_SESSION") == "1"
cancel_switch_session = os.environ.get("FAKE_PI_CANCEL_SWITCH_SESSION") == "1"
hang_new_session = os.environ.get("FAKE_PI_HANG_NEW_SESSION") == "1"


def launched_session_id():
    """The id the pool asked this process to resume, if any.

    A real pi reports the ``--session-id`` it was launched with, so a resumed
    conversation keeps the same id. This lets a test tell two processes apart.
    """
    if "--session-id" in sys.argv:
        index = sys.argv.index("--session-id") + 1
        if index < len(sys.argv):
            return sys.argv[index]
    return None


session_id = (
    os.environ.get("FAKE_PI_SESSION_ID")
    or launched_session_id()
    or "11111111-1111-5111-8111-111111111111"
)
session_file = os.environ.get(
    "FAKE_PI_SESSION_FILE", f"/tmp/fake-pi_{session_id}.jsonl"
)


def session_id_from_path(path):
    """Recover the id from a session file path like ``fake-pi_<id>.jsonl``."""
    name = os.path.basename(path)
    if name.endswith(".jsonl"):
        name = name[: -len(".jsonl")]
    if "_" in name:
        name = name.rsplit("_", 1)[-1]
    return name


def log_command(name, detail=""):
    """Record a session-management command for the pool tests to assert on."""
    if command_log:
        with open(command_log, "a") as handle:
            handle.write(name + (" " + detail if detail else "") + "\n")


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
        if cancel_new_session:
            log_command("new_session cancelled")
            print(json.dumps({"type": "response", "id": request_id,
                              "command": "new_session", "success": False,
                              "data": {"cancelled": True}}), flush=True)
            continue
        # Start a fresh session in the already-running process. The id and file
        # change so a test can tell that the pool reset the conversation.
        log_command("new_session")
        if hang_new_session:
            # Never answer; the caller's command deadline (or a cancelled run)
            # is what ends the wait.
            continue
        session_id = str(uuid.uuid4())
        session_file = f"/tmp/fake-pi_{session_id}.jsonl"
        print(json.dumps({"type": "response", "id": request_id,
                          "command": "new_session", "success": True,
                          "data": {"cancelled": False}}), flush=True)
    elif kind == "switch_session":
        path = message.get("sessionPath", "")
        if cancel_switch_session:
            log_command("switch_session cancelled", path)
            print(json.dumps({"type": "response", "id": request_id,
                              "command": "switch_session", "success": False,
                              "data": {"cancelled": True}}), flush=True)
            continue
        # Recover the id the pool switched to, so the next get_state reports it.
        restored = session_id_from_path(path)
        if restored:
            session_id = restored
            session_file = path
        log_command("switch_session", path)
        print(json.dumps({"type": "response", "id": request_id,
                          "command": "switch_session", "success": True,
                          "data": {"cancelled": False}}), flush=True)
