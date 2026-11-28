#!/usr/bin/env python3
"""Rebuild hello.com with cosmocc (https://cosmo.zip/pub/cosmocc/cosmocc.zip).

Usage: COSMOCC=/path/to/cosmocc/bin/cosmocc python3 build.py
"""

import os
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent

returncode = subprocess.run(
    [os.environ.get("COSMOCC") or "cosmocc", "-Os", "-mtiny", "-s", "-o", "hello.com", "hello.c"],
    cwd=HERE,
    check=False,
).returncode
if returncode != 0:
    sys.exit(returncode)
# cosmocc also leaves per-arch debug images next to the output.
for leftover in ("hello.com.dbg", "hello.aarch64.elf"):
    (HERE / leftover).unlink(missing_ok=True)
