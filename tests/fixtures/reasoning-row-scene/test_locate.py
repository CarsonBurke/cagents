"""Source contracts for the row scene's OCR span and row-index matching."""

import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("row_locator", Path(__file__).with_name("locate.py"))
locator = importlib.util.module_from_spec(spec)
spec.loader.exec_module(locator)


def run(text, x, y, width=40):
    return {"text": text, "x": x, "y": y, "width": width, "height": 14}


class Locator(unittest.TestCase):
    def test_control_words_split_across_spans_do_not_select_body_mentions(self):
        runs = [run("Open the full prompt below", 300, 150, 180),
                run("Show full", 300, 210, 60), run("prompt", 365, 211, 48)]
        self.assertEqual(locator.locate(runs, r"^Show full prompt\b"), (356, 218))

    def test_end_markers_accept_ocr_word_separator_without_losing_identity(self):
        runs = [run("PROMPT", 300, 150, 50), run("END:", 355, 150, 30),
                run("REPORT_END", 300, 200, 90)]
        self.assertEqual(locator.locate(runs, r"PROMPT[ _]?END\b"), (342, 157))

    def test_duplicate_spans_count_once_but_nested_rows_keep_their_index(self):
        runs = [run("Searched", 300, 150, 60), run("Searched query", 300, 151, 100),
                run("Searched query", 320, 200, 100)]
        self.assertEqual(locator.locate(runs, "Searched", 1), (370, 207))
        with self.assertRaises(ValueError):
            locator.locate(runs, "Searched", 2)

    def test_different_panes_and_lines_do_not_form_a_control(self):
        for runs in ([run("Show full", 300, 150), run("prompt", 590, 150)],
                     [run("Show full", 300, 150), run("prompt", 300, 175)]):
            with self.subTest(runs=runs), self.assertRaises(ValueError):
                locator.locate(runs, r"^Show full prompt\b")
        # Adjacent panes can leave less than one word's space between labels.
        with self.assertRaises(ValueError):
            locator.locate([run("Show full", 490, 150), run("prompt", 550, 150)],
                           r"^Show full prompt\b", right_edge=535)


if __name__ == "__main__":
    unittest.main()
