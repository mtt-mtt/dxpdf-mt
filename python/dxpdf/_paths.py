from __future__ import annotations

import os
import sys
from pathlib import Path
from typing import Iterable, Optional, Tuple, Union

PathInput = Union[str, os.PathLike]
FontPathInput = Optional[Union[PathInput, Iterable[PathInput]]]


def bundled_fonts_path() -> Path:
    """Return the read-only font directory installed inside the wheel."""

    return Path(__file__).resolve().parent / "fonts"


def _user_data_dir() -> Path:
    if sys.platform == "win32":
        return Path(os.environ.get("LOCALAPPDATA", Path.home() / "AppData" / "Local"))
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support"
    return Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local" / "share"))


def user_fonts_path(create: bool = True) -> Path:
    """Return the per-user dxpdf font directory, optionally creating it."""

    path = _user_data_dir() / "dxpdf" / "fonts"
    if create:
        path.mkdir(parents=True, exist_ok=True)
    return path


def _iter_paths(value: FontPathInput) -> Iterable[Path]:
    if value is None:
        return ()
    if isinstance(value, (str, os.PathLike)):
        return (Path(os.fsdecode(os.fspath(value))).expanduser(),)

    def paths() -> Iterable[Path]:
        for item in value:
            if not isinstance(item, (str, os.PathLike)):
                raise TypeError("font_dir entries must be strings or path-like objects")
            yield Path(os.fsdecode(os.fspath(item))).expanduser()

    return paths()


def _identity(path: Path) -> str:
    return os.path.normcase(str(path.resolve(strict=False)))


def font_search_paths(font_dir: FontPathInput = None) -> Tuple[Path, ...]:
    """Resolve the ordered, process-local font search paths for a conversion.

    Explicit paths come first, followed by ``DXPDF_FONT_DIR``, the per-user
    dxpdf directory, and finally the fonts bundled inside the installed wheel.
    Invalid explicit or environment paths are retained so conversion reports a
    useful error instead of silently falling back to a different font.
    """

    candidates = list(_iter_paths(font_dir))
    candidates.extend(
        Path(part).expanduser()
        for part in os.environ.get("DXPDF_FONT_DIR", "").split(os.pathsep)
        if part.strip()
    )

    user_path = user_fonts_path(create=False)
    if user_path.exists():
        candidates.append(user_path)

    bundled_path = bundled_fonts_path()
    if bundled_path.exists():
        candidates.append(bundled_path)

    result = []
    seen = set()
    for path in candidates:
        key = _identity(path)
        if key not in seen:
            seen.add(key)
            result.append(path)
    return tuple(result)
