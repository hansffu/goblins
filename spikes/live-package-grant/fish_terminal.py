"""Run upstream's Fish wrapper on a real terminal with a disposable workspace."""
import argparse
import os
from pathlib import Path
import signal
import subprocess
import tempfile

from runtime import HERE, snapshot


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--wrapper", type=Path, default=HERE / "result-fish/bin/goblins-fish")
    parser.add_argument("--workspace", type=Path, help="copy files into the disposable workspace")
    parser.add_argument("fish_args", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    fish_args = args.fish_args[1:] if args.fish_args[:1] == ["--"] else args.fish_args
    wrapper = args.wrapper.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="goblins-fish-") as directory:
        root = Path(directory)
        workspace = root / "workspace"
        workspace.mkdir()
        if args.workspace:
            snapshot(args.workspace, workspace)
        environment = dict(os.environ, AGENT_SANDBOX_SESSIONS_ROOT=str(root / "sessions"))
        # The shell/job owns terminal interrupts. Catch (rather than SIG_IGN)
        # here so exec restores the child's default signal disposition.
        previous = signal.signal(signal.SIGINT, lambda *_: None)
        try:
            return subprocess.call(
                [str(wrapper), "--no-config", *(fish_args or ["--interactive"])],
                cwd=workspace, env=environment)
        finally:
            signal.signal(signal.SIGINT, previous)


if __name__ == "__main__":
    raise SystemExit(main())
