#!/usr/bin/env python3
"""Verify the installed mixed Python package and its wheel contents."""

from __future__ import annotations

import argparse
import json
import zipfile
from pathlib import Path

import dxpdf


def verify(wheel: Path) -> None:
    with zipfile.ZipFile(wheel) as archive:
        names = set(archive.namelist())

    required = {
        "dxpdf/__init__.py",
        "dxpdf/_paths.py",
        "dxpdf/py.typed",
        "dxpdf/fonts/manifest.json",
        "dxpdf/fonts/licenses/README.md",
    }
    missing = sorted(required - names)
    if missing:
        raise SystemExit(f"{wheel.name}: missing wheel entries: {missing}")

    native = [
        name
        for name in names
        if name.startswith("dxpdf/_native.")
        and name.endswith((".pyd", ".so", ".dylib"))
    ]
    if len(native) != 1:
        raise SystemExit(f"{wheel.name}: expected one native extension, got {native}")

    if not callable(dxpdf.convert) or not callable(dxpdf.convert_file):
        raise SystemExit("installed dxpdf package does not expose conversion functions")

    bundled = dxpdf.bundled_fonts_path()
    manifest_path = bundled / "manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if manifest.get("schema_version") != 1 or not isinstance(manifest.get("fonts"), list):
        raise SystemExit("installed font manifest has an invalid schema")

    paths = dxpdf.font_search_paths()
    if bundled not in paths:
        raise SystemExit("bundled font directory is absent from the default search path")

    print(f"OK: {wheel.name} :: {native[0]}")
    print(f"  bundled fonts: {bundled}")
    print(f"  user fonts:    {dxpdf.user_fonts_path(create=False)}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("wheel", type=Path)
    args = parser.parse_args()
    verify(args.wheel)


if __name__ == "__main__":
    main()
