#!/usr/bin/env python3
"""Offline native-TUI stand-in. Receives only the fixed wakeup, then fetches."""
import json
import os
import subprocess
import sys
import signal
from pathlib import Path
import termios
import time
import tty

CLI = "/run/goblins/bin/goblins"
PROMPT = "Check your Goblins inbox and process the next item."

def cli(*args):
    return json.loads(subprocess.check_output([CLI, *args], stderr=subprocess.DEVNULL))

def hook(event):
    return cli("integration", "hook", "--event", event)

def ready():
    sys.stdout.write("\x1b[2J\x1b[H› \x1b[?25h")
    sys.stdout.flush()

old = termios.tcgetattr(0)
tty.setraw(0)
try:
    ready()
    text = b""
    while True:
        data = os.read(0, 4096)
        if not data:
            break
        text += data
        # Native terminals receive focus reports even when no text was typed.
        text = text.replace(b"\x1b[I", b"").replace(b"\x1b[O", b"")
        if b"\r" not in text:
            continue
        submitted, text = text.split(b"\r", 1)
        submitted = submitted.replace(b"\x1b[200~", b"").replace(b"\x1b[201~", b"").decode()
        if submitted == "exit":
            break
        hook("UserPromptSubmit")
        sys.stdout.write("\x1b[2J\x1b[HWorking (esc to interrupt)\x1b[?25l")
        sys.stdout.flush()
        if submitted == "busy":
            time.sleep(2)
        elif submitted == "notifier-fail":
            for path in Path("/proc").glob("[0-9]*/cmdline"):
                try:
                    if b"integration\x00watch\x00" in path.read_bytes():
                        os.kill(int(path.parent.name), signal.SIGTERM)
                except (OSError, ValueError):
                    pass
        elif submitted != PROMPT:
            raise RuntimeError("unexpected injected input: " + repr(submitted))
        while True:
            item = cli("inbox", "next")
            if item["message"] is None:
                if hook("Stop")["continue"]:
                    continue
                break
            cli("reply", item["message"]["id"], "--claim-generation", str(item["claim_generation"]), "--message", "processed: " + item["message"]["body"])
        ready()
finally:
    termios.tcsetattr(0, termios.TCSANOW, old)
