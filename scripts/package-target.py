#!/usr/bin/env python3
"""Stage a prebuilt sidecar for one DBX target and produce a .dbxp.

`dbx-plugin package` only packages the current host target. This helper lets a
release job (or a cross-compiling developer) package a binary built elsewhere.

The packager binary can be supplied with --packager or DBX_PACKAGER. Build it
from a DBX checkout with:

    cargo build --release --manifest-path plugins/sdk/packager/Cargo.toml
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True, help="e.g. linux-x64, windows-x64, darwin-arm64")
    parser.add_argument("--binary", required=True, help="path to the built sidecar")
    parser.add_argument("--packager", default=os.environ.get("DBX_PACKAGER"), help="dbx-plugin-packager executable")
    parser.add_argument("--output-dir", default=str(ROOT / "dist"))
    args = parser.parse_args()

    packager = args.packager
    if not packager:
        packager = shutil.which("dbx-plugin-packager")
    if not packager:
        print("Set --packager or DBX_PACKAGER to the dbx-plugin-packager binary", file=sys.stderr)
        return 2

    manifest = json.loads((ROOT / "manifest.json").read_text(encoding="utf-8"))
    version = manifest["version"]
    plugin_id = manifest["id"]
    binary = Path(args.binary).resolve()
    if not binary.is_file():
        print(f"Sidecar not found: {binary}", file=sys.stderr)
        return 2

    executable = binary.name
    if args.target.startswith("windows") and not executable.endswith(".exe"):
        executable += ".exe"

    output_dir = Path(args.output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    package_name = f"{plugin_id}-{version}-{args.target}.dbxp"
    package_path = output_dir / package_name
    metadata_path = output_dir / package_name.replace(".dbxp", ".artifact.json")

    with tempfile.TemporaryDirectory(prefix=f"dbx-notes-stage-{args.target}-") as temp:
        stage = Path(temp)
        backend = manifest.setdefault("entrypoints", {}).setdefault("backend", {})
        backend["executable"] = f"bin/{args.target}/{executable}"
        (stage / "manifest.json").write_text(
            json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )

        for include in ("assets", "ui"):
            source = ROOT / include
            if source.exists():
                shutil.copytree(source, stage / include)

        target_dir = stage / "bin" / args.target
        target_dir.mkdir(parents=True)
        shutil.copy2(binary, target_dir / executable)
        if not args.target.startswith("windows"):
            os.chmod(target_dir / executable, 0o755)

        subprocess.run(
            [
                packager,
                str(stage),
                str(package_path),
                "--artifact-metadata",
                str(metadata_path),
                "--target",
                args.target,
                "--artifact-url",
                package_name,
            ],
            check=True,
        )

    print(f"Built {package_path}")
    print(f"Metadata {metadata_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
