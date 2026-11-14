"""Editing a goblin's files in host Emacs (editor.emacs.enable): EDITOR and
the TRAMP tools in every goblin, and editor.open running a (stand-in)
emacsclient on the host."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import uuid

from build_support import fixture_app
from daemon_support import Daemon
from support import step


class EditorWorkflow(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.app = fixture_app("editor-goblins")

    def setUp(self):
        self.d = Daemon(self.app)
        self.addCleanup(self.d.close)
        temp = tempfile.TemporaryDirectory(prefix="goblins-editor-")
        self.addCleanup(temp.cleanup)
        self.dir = Path(temp.name)
        # Stands in for emacsclient: records its arguments, can block or fail.
        self.client = self.dir / "emacsclient"
        self.client.write_text(f"""#!/bin/sh
if [ "$3" = --eval ]; then
  printf '%s\\n' "$4" >> {self.dir}/eval
  if [ -e {self.dir}/conflict ]; then
    echo "*ERROR*: Goblins: /goblins:gribble: is pinned to another goblin" >&2; exit 1
  fi
  exit 0
fi
printf '%s\\n' "$@" > {self.dir}/args
echo $$ > {self.dir}/pid
while [ -e {self.dir}/block ]; do sleep 0.05; done
if [ -e {self.dir}/fail ]; then echo "emacsclient: edit aborted" >&2; exit 3; fi
""")
        self.client.chmod(0o755)
        manifest = json.loads(Path(self.d.manifest).read_text())
        manifest["editor"]["command"] = str(self.client)
        self.manifest = self.dir / "manifest.json"
        self.manifest.write_text(json.dumps(manifest))

    def start(self, name, **params):
        launch = self.d.rpc.call("sessions.start", {
            "key": uuid.uuid4().hex, "name": "plain", "agent_name": name,
            "configuration": str(self.manifest), "rows": 24, "cols": 80, **params})
        self.d.wait(lambda: self.d.get(launch["session"])["state"] == "running", timeout=60)
        return launch

    def exec(self, goblin, script, **kw):
        return subprocess.run([str(self.app), "--state-dir", str(self.d.state), "exec", goblin,
                               "--", "sh", "-c", script], capture_output=True, text=True,
                              timeout=30, **kw)

    def args(self):
        return (self.dir / "args").read_text().splitlines()

    def test_edit_a_goblins_files_in_host_emacs(self):
        with step("EDITOR and the TRAMP tools reach goblins in and out of a scope"):
            launch = self.start("gribble")
            self.start("scoped", scope="work")
            for goblin in ("gribble", "scoped"):
                r = self.exec(goblin, 'echo "$EDITOR $VISUAL"; for t in sed awk find grep stat base64; '
                                      'do command -v $t >/dev/null || echo "missing $t"; done')
                # A goblin's own VISUAL wins over the editor default.
                self.assertEqual(r.stdout, "/run/goblins/bin/goblins-editor custom-visual\n", r.stderr)

        with step("files open at their positions, through the goblin's own Emacs connection"):
            r = self.exec("gribble", "cd /tmp && $EDITOR --no-wait +3:4 a.txt /etc/x +9 ../tmp/./c")
            self.assertEqual(r.returncode, 0, r.stderr)
            # First the host is pinned in Emacs to this session of this daemon.
            self.assertEqual((self.dir / "eval").read_text(),
                             f'(goblins-pin-host "gribble" "{self.d.state}" '
                             f'"{self.d.rpc.init["instance"]}" "{launch["session"]}")\n')
            self.assertEqual(self.args(), ["-n", "-a", "false", "+3:4", "/goblins:gribble:/tmp/a.txt",
                                           "/goblins:gribble:/etc/x", "+9", "/goblins:gribble:/tmp/c"])

        with step("by default an edit blocks until the editor is done"):
            (self.dir / "block").touch()
            editor = subprocess.Popen([str(self.app), "--state-dir", str(self.d.state), "exec",
                                       "gribble", "--", "goblin", "editor", "open", "/tmp/msg"])
            self.addCleanup(lambda: editor.poll() is None and (editor.kill(), editor.wait(timeout=10)))
            self.d.wait(lambda: (self.dir / "args").exists() and self.args()[-1] == "/goblins:gribble:/tmp/msg")
            self.assertEqual(self.args(), ["-a", "false", "/goblins:gribble:/tmp/msg"])
            self.assertIsNone(editor.poll())
            (self.dir / "block").unlink()
            self.assertEqual(editor.wait(timeout=10), 0)

        with step("a name Emacs has pinned to another goblin opens nothing"):
            (self.dir / "args").unlink()
            (self.dir / "conflict").touch()
            r = self.exec("gribble", "goblins-editor /tmp/msg")
            self.assertEqual(r.returncode, 1)
            self.assertIn("/goblins:gribble: is pinned to another goblin", r.stderr)
            self.assertFalse((self.dir / "args").exists())
            (self.dir / "conflict").unlink()

        with step("an aborted edit fails, with the editor's explanation"):
            (self.dir / "fail").touch()
            r = self.exec("gribble", "goblins-editor /tmp/msg")
            self.assertEqual(r.returncode, 3)
            self.assertIn("emacsclient: edit aborted", r.stderr)
            (self.dir / "fail").unlink()

        with step("a helper edits under its own name"):
            # A member gets the editor of the manifest its owner was started from.
            self.d.member(launch["session"], name="plain", agent_name="snaga")
            r = self.exec("snaga", "goblins-editor -n /x")
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(self.args()[-1], "/goblins:snaga:/x")

        with step("cancelling a waiting edit kills the editor client"):
            # The client is killed instead of waiting on a goblin nobody watches.
            (self.dir / "block").touch()
            (self.dir / "pid").unlink()
            waiting = subprocess.Popen([str(self.app), "--state-dir", str(self.d.state), "exec",
                                        "gribble", "--", "goblins-editor", "/tmp/y"])
            self.d.wait(lambda: (self.dir / "pid").exists())
            pid = int((self.dir / "pid").read_text())
            waiting.kill()
            waiting.wait()
            self.d.wait(lambda: not Path(f"/proc/{pid}").exists())

        with step("a goblin that stops ends its pending edits"):
            (self.dir / "pid").unlink()
            waiting = subprocess.Popen([str(self.app), "--state-dir", str(self.d.state), "exec",
                                        "snaga", "--", "goblins-editor", "/tmp/z"],
                                       stderr=subprocess.DEVNULL)
            self.addCleanup(lambda: waiting.poll() is None and (waiting.kill(), waiting.wait(timeout=10)))
            self.d.wait(lambda: (self.dir / "pid").exists())
            pid = int((self.dir / "pid").read_text())
            self.d.rpc.call("sessions.stop", {"session": "snaga"})
            self.d.wait(lambda: not Path(f"/proc/{pid}").exists())
            self.assertNotEqual(waiting.wait(timeout=10), 0)


if __name__ == "__main__":
    unittest.main()
