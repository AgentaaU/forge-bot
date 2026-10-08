#!/usr/bin/env python3
"""Regression check for the crates.io mirror probe in the CI toolchain step.

The script extracts the real `run:` block of the "Set up pinned Rust toolchain"
step from .github/workflows/ci.yml, stops right before the rustup installer
(no network installs), and runs it under the runner's shell flags
(`bash --noprofile --norc -e -o pipefail`) against a local fake mirror.

Every scenario must exit 0, reach the sentinel that follows the crates block,
and leave CARGO_HOME/config.toml either absent (crates.io) or pointing at the
mirror only when the mirror is fully healthy.
"""

import http.server
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import threading

ROOT = pathlib.Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
SENTINEL = "CRATES_BLOCK_DONE"

# Scenario name -> (GET config.json behaviour, HEAD/GET of download host ok?,
# expect mirror config written?)
SCENARIOS = {
    "healthy": ("ok", True, True),
    "head-200-get-503": ("503", True, False),
    "head-200-get-disconnect": ("disconnect", True, False),
    "head-200-get-garbage": ("garbage", True, False),
    "download-host-404": ("ok", False, False),
}


def extract_crates_block():
    # Plain-text extraction (no PyYAML dependency) of the step's `run: |` block.
    lines = WORKFLOW.read_text().splitlines()
    start = next(i for i, line in enumerate(lines)
                 if line.strip() == "- name: Set up pinned Rust toolchain")
    run = next(i for i in range(start, len(lines)) if lines[i].strip() == "run: |")
    indent = len(lines[run]) - len(lines[run].lstrip()) + 2
    body = []
    for line in lines[run + 1:]:
        if line.strip() and len(line) - len(line.lstrip()) < indent:
            break
        body.append(line[indent:] if line.strip() else "")
    cut = next(i for i, line in enumerate(body) if line.startswith("curl --proto"))
    # Keep only the lines before the rustup installer, then append the sentinel.
    return "\n".join(body[:cut] + [f"echo {SENTINEL}"]) + "\n"


def make_handler(mode, dl_ok, port):
    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def _config_body(self):
            if mode == "garbage":
                return b"<html>not json</html>"
            return (
                '{"dl": "http://127.0.0.1:%d/api/v1/crates", "api": "x"}' % port
            ).encode()

        def do_HEAD(self):
            self._respond(head=True)

        def do_GET(self):
            self._respond(head=False)

        def _respond(self, head):
            if self.path == "/config.json":
                if head:
                    return self._send(200, b"")
                if mode == "503":
                    return self._send(503, b"unavailable")
                if mode == "disconnect":
                    self.close_connection = True
                    return  # no response written: client sees an empty reply
                return self._send(200, self._config_body())
            if self.path.startswith("/api/v1/crates/serde/1.0.0/download"):
                return self._send(200 if dl_ok else 404, b"" if head else b"x")
            self._send(404, b"")

        def _send(self, code, body):
            self.send_response(code)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if not self.command == "HEAD":
                self.wfile.write(body)

    return Handler


def run_scenario(name, mode, dl_ok, expect_mirror, script, tmp):
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), None)
    port = server.server_address[1]
    server.RequestHandlerClass = make_handler(mode, dl_ok, port)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        work = tmp / name
        runner_temp = work / "runner"
        runner_temp.mkdir(parents=True)
        (work / "repo").mkdir()
        shutil.copy(ROOT / "rust-toolchain.toml", work / "repo" / "rust-toolchain.toml")
        env = {
            "PATH": os.environ["PATH"],
            "HOME": str(work),
            "RUNNER_TEMP": str(runner_temp),
            "GITHUB_ENV": str(work / "github_env"),
            "GITHUB_PATH": str(work / "github_path"),
            # Disable the Rust dist mirror probe; this check only covers crates.
            "RUST_MIRROR": "",
            "CRATES_MIRROR": f"http://127.0.0.1:{port}/",
        }
        for path in ("github_env", "github_path"):
            (work / path).touch()
        result = subprocess.run(
            ["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", script],
            cwd=work / "repo",
            env=env,
            capture_output=True,
            text=True,
            timeout=120,
        )
        errors = []
        if result.returncode != 0:
            errors.append(f"exit {result.returncode}")
        if SENTINEL not in result.stdout:
            errors.append("setup did not continue past the crates block")
        cargo_cfg = None
        for line in (work / "github_env").read_text().splitlines():
            if line.startswith("CARGO_HOME="):
                cargo_cfg = pathlib.Path(line.split("=", 1)[1]) / "config.toml"
        written = cargo_cfg is not None and cargo_cfg.exists()
        if written != expect_mirror:
            errors.append(f"mirror config written={written}, expected {expect_mirror}")
        if written and f"127.0.0.1:{port}" not in cargo_cfg.read_text():
            errors.append("mirror config does not point at the mirror")
        status = "PASS" if not errors else "FAIL"
        print(f"{status} {name}")
        if errors:
            print("  " + "; ".join(errors))
            print("  stdout:\n" + result.stdout)
            print("  stderr:\n" + result.stderr)
        return not errors
    finally:
        server.shutdown()
        server.server_close()


def main():
    script = extract_crates_block()
    tmp = pathlib.Path(tempfile.mkdtemp(prefix="crates-mirror-test."))
    try:
        results = [
            run_scenario(name, *params, script, tmp)
            for name, params in SCENARIOS.items()
        ]
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    # Empty CRATES_MIRROR (disabled) is covered by the fallback path too.
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
