"""Deterministic startup-bind races driven by a concurrent writable sandbox."""
import json
import os
from pathlib import Path
import shlex
import shutil
import sys
import tempfile
import unittest
from daemon_support import Daemon
from build_support import fixture_app


class BindRaceTests(unittest.TestCase):
    def exercise(self, directory, readonly, ancestor):
        with tempfile.TemporaryDirectory(prefix="gbr-") as tmp:
            root = Path(tmp)
            shared = root / "shared"; shared.mkdir()
            bindir = root / "bin"; bindir.mkdir()
            armed, paused, release = [root / n for n in ("armed", "paused", "release")]
            real = shutil.which("nix-store")
            wrapper = bindir / "nix-store"
            wrapper.write_text(f'''#!{sys.executable}
import os,sys,time
if "--realise" in sys.argv and os.path.exists({str(armed)!r}) and not os.path.exists({str(paused)!r}):
 open({str(paused)!r},"w").close()
 for _ in range(6000):
  if os.path.exists({str(release)!r}): break
  time.sleep(.01)
 else: sys.exit("bind-race barrier timed out")
os.execv({real!r},[{real!r},*sys.argv[1:]])
'''); wrapper.chmod(0o700)
            # The attacker only executes POSIX commands over a raw socket.
            # Bash avoids waiting for Fish terminal-query replies from a
            # deliberately non-emulating reader on every race variant.
            d = Daemon(fixture_app("exec-goblins"),
                       env={**os.environ, "PATH": str(bindir) + ":" + os.environ["PATH"]})
            try:
                (d.state / "sentinel").write_text("PRIVATE-CONTROL-DATA")
                if ancestor:
                    parent = shared / "parent"; parent.mkdir()
                    source = parent / (d.state.name if directory else "sentinel")
                    replacement = d.state.parent if directory else d.state
                    swap = parent
                else:
                    source = shared / "child"
                    replacement = d.state if directory else d.state / "sentinel"
                    swap = source
                if directory:
                    source.mkdir(); (source / "original").write_text("ORIGINAL-GRANT")
                else:
                    source.write_text("ORIGINAL-GRANT")
                original_inode = (source.stat().st_dev, source.stat().st_ino)

                def config(name, option, path):
                    manifest = json.loads(Path(d.manifest).read_text())
                    spec = json.loads(Path(manifest["goblins"]["shell"]["build_spec"]).read_text())
                    for key in ("rw_dirs", "ro_dirs", "rw_files", "ro_files"): spec[key] = []
                    spec[option] = [str(path)]
                    specpath = root / (name + "-spec.json"); specpath.write_text(json.dumps(spec))
                    manifest["goblins"]["shell"]["build_spec"] = str(specpath)
                    path = root / (name + ".json"); path.write_text(json.dumps(manifest))
                    return path

                a = d.start(manifest=config("a", "rw_dirs", shared))
                with d.terminal(a) as attacker:
                    armed.touch()
                    option = ("ro_" if readonly else "rw_") + ("dirs" if directory else "files")
                    b = d.start(manifest=config("b", option, source), wait=False)
                    d.wait(paused.exists)  # plan() completed; Nix startup is blocked.
                    saved = shared / "saved"
                    attacker.sendall(f"mv {shlex.quote(str(swap))} {shlex.quote(str(saved))}; ln -s {shlex.quote(str(replacement))} {shlex.quote(str(swap))}; printf 'SWAPPED\\n'\n".encode())
                    d.wait(swap.is_symlink)
                    self.assertTrue(swap.is_symlink())
                    self.assertEqual(source.resolve(), d.state if directory else d.state / "sentinel")
                    release.touch()
                    record = d.wait(lambda: r if (r := d.get(b["session"]))["state"] != "starting" else None)
                    self.assertEqual(record["state"], "running", record)
                    payload_root = Path(f"/proc/{record['identity']['pid']}/root")
                    mounted = payload_root / source.relative_to("/")
                    exposed = mounted / "sentinel" if directory else mounted
                    self.assertFalse(directory and (mounted / "host.sock").exists(), "B received the daemon host socket")
                    self.assertEqual((mounted.stat().st_dev, mounted.stat().st_ino), original_inode)
                    self.assertEqual((mounted / "original" if directory else mounted).read_text(), "ORIGINAL-GRANT")
                    self.assertNotEqual(exposed.read_text() if exposed.is_file() else "", "PRIVATE-CONTROL-DATA")
                    # Inspect the namespace-init, goblins-init and actual shell
                    # descriptor tables. No O_PATH source handle may survive.
                    payload_pids, queue = [], [record["identity"]["pid"]]
                    while queue:
                        pid = queue.pop()
                        payload_pids.append(pid)
                        try:
                            queue.extend(map(int, Path(f"/proc/{pid}/task/{pid}/children").read_text().split()))
                        except FileNotFoundError:  # a short-lived helper of the shell
                            pass
                    self.assertGreaterEqual(len(payload_pids), 3)
                    for pid in payload_pids:
                        try:
                            command = Path(f"/proc/{pid}/cmdline").read_bytes()
                        except FileNotFoundError:
                            continue
                        if command.startswith(b"/run/goblins/bin/goblins-init\0"):
                            # Not dumpable, so its table is hidden from this
                            # uid; it inherits only what the shell inherits.
                            with self.assertRaises(PermissionError):
                                next(Path(f"/proc/{pid}/fd").iterdir())
                            continue
                        try:
                            fds = list(Path(f"/proc/{pid}/fd").iterdir())
                        except FileNotFoundError:
                            continue
                        for fd in fds:
                            try: meta = fd.stat()
                            except FileNotFoundError: continue
                            self.assertNotEqual((meta.st_dev, meta.st_ino), original_inode, f"source FD leaked to {pid}: {fd}")
                    # The grant is a live bind of the pinned object, not a copy.
                    actual = saved / source.name if ancestor else saved
                    (actual / "original" if directory else actual).write_text("HOST-UPDATE")
                    self.assertEqual((mounted / "original" if directory else mounted).read_text(), "HOST-UPDATE")
                print(f"EVIDENCE {option} {ancestor=}: sandbox A replaced the source; B retained the validated inode, no host socket or source FDs", flush=True)
            finally:
                release.touch()
                d.close()

    def test_directory_leaf_and_ancestor_replacement(self):
        for readonly in (True, False):
            for ancestor in (False, True):
                with self.subTest(readonly=readonly, ancestor=ancestor):
                    self.exercise(True, readonly, ancestor)

    def test_file_leaf_and_ancestor_replacement(self):
        for readonly in (True, False):
            for ancestor in (False, True):
                with self.subTest(readonly=readonly, ancestor=ancestor):
                    self.exercise(False, readonly, ancestor)


if __name__ == "__main__": unittest.main(verbosity=2)
