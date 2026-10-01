#!/usr/bin/env python3
"""Echo every incoming JSON line to ``FAKE_ECHO_LOG``.

Used by the `wire` transport tests to observe what the client writes without
implementing any protocol. The process stays alive until stdin closes.
"""

import os
import sys

log = os.environ["FAKE_ECHO_LOG"]
with open(log, "w"):
    pass
for line in sys.stdin:
    with open(log, "a") as handle:
        handle.write(line)
