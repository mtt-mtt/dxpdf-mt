import importlib
import os
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock


SOURCE_PYTHON = Path(__file__).resolve().parents[1]


class NativeStub(types.ModuleType):
    def __init__(self) -> None:
        super().__init__("dxpdf._native")
        self.calls = []

    def convert(self, docx_bytes, **kwargs):
        self.calls.append(("convert", docx_bytes, kwargs))
        return b"pdf"

    def convert_file(self, input, output, **kwargs):
        self.calls.append(("convert_file", input, output, kwargs))


class PackageApiTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        sys.path.insert(0, str(SOURCE_PYTHON))
        cls.native = NativeStub()
        sys.modules["dxpdf._native"] = cls.native
        cls.dxpdf = importlib.import_module("dxpdf")
        cls.paths = importlib.import_module("dxpdf._paths")

    @classmethod
    def tearDownClass(cls) -> None:
        sys.path.remove(str(SOURCE_PYTHON))
        for name in ("dxpdf._paths", "dxpdf._native", "dxpdf"):
            sys.modules.pop(name, None)

    def setUp(self) -> None:
        self.native.calls.clear()

    def test_search_paths_preserve_priority_and_deduplicate(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            explicit_a = root / "explicit-a"
            explicit_b = root / "explicit-b"
            environment = root / "environment"
            user = root / "user"
            bundled = root / "bundled"
            for path in (explicit_a, explicit_b, environment, user, bundled):
                path.mkdir()

            with mock.patch.dict(
                os.environ,
                {"DXPDF_FONT_DIR": os.pathsep.join((str(explicit_b), str(environment)))},
                clear=False,
            ), mock.patch.object(
                self.paths, "user_fonts_path", return_value=user
            ), mock.patch.object(
                self.paths, "bundled_fonts_path", return_value=bundled
            ):
                result = self.paths.font_search_paths([explicit_a, explicit_b])

            self.assertEqual(
                result,
                (explicit_a, explicit_b, environment, user, bundled),
            )

    def test_user_fonts_path_can_create_the_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir, mock.patch.object(
            self.paths, "_user_data_dir", return_value=Path(temp_dir)
        ):
            result = self.paths.user_fonts_path()
            self.assertTrue(result.is_dir())
            self.assertEqual(result, Path(temp_dir) / "dxpdf" / "fonts")

    def test_convert_forwards_all_resolved_font_paths(self) -> None:
        expected = (Path("first"), Path("second"))
        with mock.patch.object(self.dxpdf, "font_search_paths", return_value=expected):
            result = self.dxpdf.convert(b"docx", image_dpi=300, font_dir="ignored")

        self.assertEqual(result, b"pdf")
        self.assertEqual(
            self.native.calls,
            [
                (
                    "convert",
                    b"docx",
                    {"image_dpi": 300, "font_dirs": ["first", "second"]},
                )
            ],
        )

    def test_convert_file_accepts_pathlike_values(self) -> None:
        with mock.patch.object(self.dxpdf, "font_search_paths", return_value=()):
            self.dxpdf.convert_file(Path("input.docx"), Path("output.pdf"))

        self.assertEqual(
            self.native.calls,
            [
                (
                    "convert_file",
                    "input.docx",
                    "output.pdf",
                    {"image_dpi": 220.0, "font_dirs": None},
                )
            ],
        )

    def test_non_path_entries_are_rejected(self) -> None:
        with self.assertRaisesRegex(TypeError, "font_dir entries"):
            tuple(self.paths.font_search_paths([Path("valid"), 42]))


if __name__ == "__main__":
    unittest.main()
