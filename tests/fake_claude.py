#!/usr/bin/env python3
"""Offline native-TUI stand-in. Receives only the fixed wakeup, then fetches."""
import json
import os
import subprocess
import sys
import signal
import select
from pathlib import Path
import termios
import time
import tty
import uuid

CLI = "/run/goblins/bin/goblin"
PROMPT = "Check your Goblins inbox and process the next item."
COMPOSER = os.environ.get("GOBLINS_TEST_COMPOSER", "› ")

def cli(*args):
    return json.loads(subprocess.check_output([CLI, *args], stderr=subprocess.DEVNULL))

# The real Goblins hook, with the payload fields Claude Code sends: a
# conversation id, kept on --resume as Claude does, and its transcript.
SETTINGS = json.loads(Path("/etc/claude-code/managed-settings.json").read_text())
RESUMED = "--resume" in sys.argv
SESSION = sys.argv[sys.argv.index("--resume") + 1] if RESUMED else str(uuid.uuid4())
TRANSCRIPT = Path("/tmp/fake-claude-transcript.jsonl")
# Every monitor line, for the tests to read back.
WAKES = Path("/tmp/fake-claude-wakes")
TRANSCRIPT.write_text(json.dumps({"type": "ai-title", "aiTitle": "Resumed task" if RESUMED else "Fake task",
                                  "sessionId": SESSION}) + "\n")

def hook(event, **extra):
    command = SETTINGS["hooks"][event][0]["hooks"][0]["command"]
    payload = {"hook_event_name": event, "session_id": SESSION, "transcript_path": str(TRANSCRIPT), **extra}
    return json.loads(subprocess.check_output([command], input=json.dumps(payload).encode(), stderr=subprocess.DEVNULL))

def ready():
    sys.stdout.write("\x1b[2J\x1b[H" + COMPOSER + "\x1b[?25h")
    sys.stdout.flush()

old = termios.tcgetattr(0)
tty.setraw(0)
monitor = None
try:
    hook("SessionStart")
    if "--plugin-dir" in sys.argv:
        # The plugin's own monitor, as Claude Code runs it.
        plugin = Path(sys.argv[sys.argv.index("--plugin-dir") + 1])
        [spec] = json.loads((plugin / "monitors/monitors.json").read_text())
        monitor = subprocess.Popen(
            [spec["command"]],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
    ready()
    text = b""
    while True:
        sources = [0] + ([monitor.stdout] if monitor is not None else [])
        readable, _, _ = select.select(sources, [], [])
        from_monitor = monitor is not None and monitor.stdout in readable
        if from_monitor:
            notification = monitor.stdout.readline()
            if not notification:
                monitor = None
                continue
            submitted = notification.decode().rstrip("\n")
            with WAKES.open("a") as wakes:
                wakes.write(submitted + "\n")
        else:
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
        if from_monitor:
            # Claude Code hands a monitor line to the turn as a task notification.
            hook("UserPromptSubmit", prompt=f"<task-notification>\n<task-id>monitor</task-id>\n"
                 f"<summary>Monitor event: \"{spec['description']}\"</summary>\n"
                 f"<event>{submitted}</event>\n</task-notification>")
        else:
            hook("UserPromptSubmit")
        sys.stdout.write("\x1b[2J\x1b[HWorking (esc to interrupt)\x1b[?25l")
        sys.stdout.flush()
        if submitted == "busy":
            time.sleep(2)
        elif submitted == "long-busy":
            time.sleep(8)
        elif submitted == "notifier-fail":
            for path in Path("/proc").glob("[0-9]*/cmdline"):
                try:
                    if b"goblins-claude\x00watch\x00" in path.read_bytes():
                        os.kill(int(path.parent.name), signal.SIGTERM)
                except (OSError, ValueError):
                    pass
        elif not (from_monitor and submitted.startswith(PROMPT + " [goblins-inbox-wake")):
            raise RuntimeError("unexpected injected input: " + repr(submitted))
        # The wake line names the message it is about: the one fetched first.
        named = submitted.removeprefix(PROMPT + " [goblins-inbox-wake").rstrip("]").strip() if from_monitor else ""
        while True:
            item = cli("inbox", "next")
            if named and item["message"] is not None and item["message"]["id"] != named:
                raise RuntimeError(f"wake named {named}, fetched {item['message']['id']}")
            named = ""
            if item["message"] is None:
                if hook("Stop").get("decision") == "block":
                    continue
                break
            if item["message"].get("operation"):
                # Operation notices take no reply.
                cli("inbox", "complete", item["message"]["id"], "--claim-generation", str(item["claim_generation"]))
            else:
                cli("reply", item["message"]["id"], "--claim-generation", str(item["claim_generation"]), "--message", "processed: " + item["message"]["body"])
        ready()
finally:
    if monitor is not None:
        monitor.terminate()
        monitor.wait()
    termios.tcsetattr(0, termios.TCSANOW, old)
