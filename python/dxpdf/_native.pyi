from typing import List, Optional


def convert(
    docx_bytes: bytes,
    image_dpi: float = 220.0,
    font_dirs: Optional[List[str]] = None,
) -> bytes: ...


def convert_file(
    input: str,
    output: str,
    image_dpi: float = 220.0,
    font_dirs: Optional[List[str]] = None,
) -> None: ...
