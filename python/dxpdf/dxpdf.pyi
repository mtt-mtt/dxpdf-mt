from typing import Optional


def convert(
    docx_bytes: bytes,
    image_dpi: float = 220.0,
    font_dir: Optional[str] = None,
) -> bytes:
    """Convert DOCX bytes to PDF bytes.

    Args:
        docx_bytes: Raw bytes of a .docx file.
        image_dpi: Target DPI for embedded raster images.
        font_dir: Optional controlled font-pack directory.

    Returns:
        PDF file contents as bytes.

    Raises:
        RuntimeError: If the DOCX file is invalid or conversion fails.
    """
    ...

def convert_file(
    input: str,
    output: str,
    image_dpi: float = 220.0,
    font_dir: Optional[str] = None,
) -> None:
    """Convert a DOCX file to a PDF file.

    Args:
        input: Path to the input .docx file.
        output: Path to the output .pdf file.
        image_dpi: Target DPI for embedded raster images.
        font_dir: Optional controlled font-pack directory.

    Raises:
        RuntimeError: If reading, conversion, or writing fails.
    """
    ...
