"""Focused acceptance checks for the independent smoke catalog oracle."""

import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from mcp_catalog import load_tool_names, tool_names_match


class CatalogTests(unittest.TestCase):
    def test_checked_in_catalog_matches_exact_names(self):
        names = load_tool_names()
        self.assertTrue(names)
        self.assertTrue(tool_names_match(reversed(names), names))

    def test_missing_extra_and_duplicate_actual_names_fail(self):
        names = load_tool_names()
        for actual in (names[1:], names + ("unexpected_tool",), names[:-1] + (names[0],)):
            with self.subTest(actual=actual):
                self.assertFalse(tool_names_match(actual, names))

    def test_malformed_oracle_is_refused(self):
        bodies = ("{", "{}", "[]", '["tool"]', '[{"name": 7}]',
                  '[{"name": "tool"}, {"name": "tool"}]')
        snapshots = ["---\nsource: fixture\n---\n" + body for body in bodies]
        snapshots += ["[]", "---\nsource: fixture\n[]"]
        for snapshot in snapshots:
            with self.subTest(snapshot=snapshot):
                with patch.object(Path, "read_text", return_value=snapshot):
                    with self.assertRaises(ValueError):
                        load_tool_names()

    def test_missing_oracle_is_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(FileNotFoundError):
                load_tool_names(Path(directory) / "missing.snap")

    def test_lookup_is_independent_of_caller_cwd(self):
        expected = load_tool_names()
        original = os.getcwd()
        with tempfile.TemporaryDirectory() as directory:
            try:
                os.chdir(directory)
                self.assertEqual(load_tool_names(), expected)
            finally:
                os.chdir(original)


if __name__ == "__main__":
    unittest.main()
