#!/usr/bin/env python3
"""Render Mermaid blocks in the architecture Org files using mermaid-cli."""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import shlex
import subprocess
import tempfile
import xml.etree.ElementTree as ET
from xml.sax.saxutils import escape


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mmdc", default="mmdc", help="Mermaid CLI executable")
    parser.add_argument("--config", type=Path,
                        default=Path(__file__).resolve().parent / "mermaid-config.json",
                        help="Mermaid JSON configuration file")
    parser.add_argument("--font-dir", type=Path,
                        help="font directory for environments without system fonts")
    args = parser.parse_args()
    renderer = shutil.which(args.mmdc)
    if renderer is None:
        parser.error(f"renderer not found: {args.mmdc}; install mermaid-cli")

    root = Path(__file__).resolve().parent
    output = root / "diagrams"
    output.mkdir(exist_ok=True)
    count = 0
    with tempfile.TemporaryDirectory(prefix="goblins-mermaid-") as scratch:
        scratch = Path(scratch)
        # Chromium runs inside the outer Goblins sandbox. Its nested sandbox
        # cannot assume that namespace creation is available there.
        puppeteer = scratch / "puppeteer.json"
        puppeteer.write_text(json.dumps({"args": ["--no-sandbox"]}) + "\n")
        env = os.environ.copy()
        if args.font_dir:
            if not args.font_dir.is_dir():
                parser.error(f"font directory not found: {args.font_dir}")
            fonts = scratch / "fonts.conf"
            fonts.write_text(
                '<?xml version="1.0"?><fontconfig>'
                f'<dir>{escape(str(args.font_dir.resolve()))}</dir>'
                f'<cachedir>{escape(str(scratch / "font-cache"))}</cachedir>'
                '</fontconfig>\n'
            )
            env["FONTCONFIG_FILE"] = str(fonts)
        for source in sorted(root.glob("*.org")):
            text = source.read_text()
            for match in re.finditer(
                r"(?ms)^#\+begin_src mermaid([^\n]*)\n(.*?)^#\+end_src$", text
            ):
                headers = shlex.split(match[1])
                if ":file" not in headers:
                    raise ValueError(f"Mermaid block in {source} needs a :file target")
                target = (source.parent / headers[headers.index(":file") + 1]).resolve()
                if target.parent != output or target.suffix != ".svg":
                    raise ValueError(f"Expected an SVG target under {output}: {target}")
                diagram = scratch / f"{target.stem}.mmd"
                diagram.write_text(match[2])
                subprocess.run(
                    [renderer, "--input", str(diagram), "--output", str(target),
                     "--puppeteerConfigFile", str(puppeteer),
                     "--configFile", str(args.config.resolve()), "--theme", "dark",
                     "--backgroundColor", "#282c34"],
                    check=True,
                    env=env,
                )
                svg = ET.parse(target).getroot()
                if any(element.tag.endswith("}foreignObject") for element in svg.iter()):
                    raise RuntimeError(f"HTML labels in {target} cannot display in Emacs")
                if not any(element.tag.endswith("}text") for element in svg.iter()):
                    raise RuntimeError(f"No native SVG text labels in {target}")
                for element in svg.iter():
                    for name in ("transform", "viewBox", "x", "y", "width", "height", "d"):
                        value = element.get(name, "")
                        if "NaN" in value or "undefined" in value:
                            raise RuntimeError(
                                f"Invalid SVG coordinates in {target}; "
                                "provide installed fonts with --font-dir"
                            )
                print(target.relative_to(root), flush=True)
                count += 1
    print(f"Rendered {count} diagrams.")


if __name__ == "__main__":
    main()
