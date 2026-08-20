"""Python API for dxpdf."""

from __future__ import annotations

import os
from importlib.metadata import PackageNotFoundError, version
from typing import List, Optional, Union

from . import _native
from ._paths import (
    FontPathInput,
    bundled_fonts_path,
    font_search_paths,
    user_fonts_path,
)

try:
    __version__ = version("dxpdf")
except PackageNotFoundError:  # pragma: no cover - source-tree imports only
    __version__ = "0.0.0"


def _native_font_dirs(font_dir: FontPathInput) -> Optional[List[str]]:
    paths = font_search_paths(font_dir)
    return [os.fsdecode(os.fspath(path)) for path in paths] or None


def convert(
    docx_bytes: bytes,
    image_dpi: float = 220.0,
    font_dir: FontPathInput = None,
) -> bytes:
    """Convert DOCX bytes to PDF bytes.

    ``font_dir`` may be one path or an iterable of paths. These paths are
    additive and take priority over fonts configured through the environment,
    the per-user font directory, bundled fonts, and system fallback.
    """

    return _native.convert(
        docx_bytes,
        image_dpi=image_dpi,
        font_dirs=_native_font_dirs(font_dir),
    )


def convert_file(
    input: Union[os.PathLike, str],
    output: Union[os.PathLike, str],
    image_dpi: float = 220.0,
    font_dir: FontPathInput = None,
) -> None:
    """Convert a DOCX file to a PDF file."""

    _native.convert_file(
        os.fsdecode(os.fspath(input)),
        os.fsdecode(os.fspath(output)),
        image_dpi=image_dpi,
        font_dirs=_native_font_dirs(font_dir),
    )


__all__ = [
    "__version__",
    "bundled_fonts_path",
    "convert",
    "convert_file",
    "font_search_paths",
    "user_fonts_path",
]
