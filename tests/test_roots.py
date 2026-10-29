"""Initial GC retention must stay bounded without granting extra store paths."""
import json
from pathlib import Path
import tempfile
import unittest

from build_support import fixture_app, fixture_path
from daemon_support import Daemon, receive
from support import FLAKE, command


class RootTests(unittest.TestCase):
    def test_demo_initial_closure_is_retained_by_a_single_listing(self):
        daemon = Daemon(fixture_app("demo-goblins"))
        self.addCleanup(daemon.close)
        session = daemon.start()["session"]
        configuration = json.loads(Path(daemon.manifest).read_text())["goblins"]["shell"]
        spec = json.loads(Path(configuration["build_spec"]).read_text())
        listing = Path(spec["closure_paths_file"])
        directory = daemon.state / session / "resources"
        roots = list((directory / "roots").iterdir())
        self.assertIn(listing, [p.resolve() for p in roots])
        # Listing, host-only mount helper, and online network helper. This
        # catches a return to rooting the demo's hundred-plus paths separately.
        self.assertLessEqual(len(roots), 4)
        retained = set(command(["nix-store", "--query", "--requisites", *roots]).splitlines())
        self.assertTrue(set(listing.read_text().splitlines()) <= retained)
        daemon.rpc.call("sessions.stop", {"session": session})
        daemon.wait(lambda: not directory.exists())
        self.assertTrue(all(not root.exists() for root in roots))

    def test_gc_references_do_not_widen_mounts_and_later_grants_get_roots(self):
        daemon = Daemon(fixture_app("exec-goblins"))
        self.addCleanup(daemon.close)
        reference = fixture_path(FLAKE + "#checks.x86_64-linux.root-reference-fixture")
        unlisted = (reference / "unlisted").read_text().strip()
        configuration = json.loads(Path(daemon.manifest).read_text())
        goblin = configuration["goblins"]["shell"]
        spec = json.loads(Path(goblin["build_spec"]).read_text())
        with tempfile.TemporaryDirectory(prefix="goblins-root-test-") as temporary:
            temporary = Path(temporary)
            spec["closure_paths_file"] = str(reference / "paths")
            (temporary / "spec.json").write_text(json.dumps(spec))
            goblin["build_spec"] = str(temporary / "spec.json")
            (temporary / "manifest.json").write_text(json.dumps(configuration))
            session = daemon.start(manifest=temporary / "manifest.json")["session"]
        roots = daemon.state / session / "resources/roots"
        self.assertIn(reference, [p.resolve() for p in roots.iterdir()])
        self.assertEqual(daemon.cli("exec", session, "--", "test", "-e", unlisted,
                                   check=False).returncode, 1)
        self.assertEqual(daemon.cli("exec", session, "--", "test", "-e", reference,
                                   check=False).returncode, 1)
        before = set(roots.iterdir())
        peer, request = daemon.pending(session, "hello")
        self.addCleanup(peer.close)
        daemon.decide(request, True)
        peer.peer.settimeout(60)
        self.assertEqual(receive(peer.peer)["result"]["status"], "ready")
        self.assertGreater(len(set(roots.iterdir()) - before), 0)
        self.assertEqual(daemon.cli("exec", session, "--", "hello").stdout, "Hello, world!\n")
        self.assertEqual(daemon.cli("exec", session, "--", "test", "-e", unlisted,
                                   check=False).returncode, 1)
