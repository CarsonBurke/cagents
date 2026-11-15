"""Locate a transcript row in native-evidence OCR without crossing pane gaps."""

import json
import re
import sys


def locate(runs, expression, index=0, right_edge=810):
    pattern = re.compile(expression, re.I)
    visible = sorted((run for run in runs
                      if 270 <= run["x"] < right_edge and 55 < run["y"] < 780),
                     key=lambda run: (run["y"], run["x"]))
    lines = []
    for run in visible:
        if not lines or run["y"] - lines[-1][0]["y"] > 8:
            lines.append([])
        lines[-1].append(run)
    matches = []
    for line in lines:
        fragments = []
        for run in sorted(line, key=lambda run: run["x"]):
            if (not fragments or run["x"] >
                    max(part["x"] + part["width"] for part in fragments[-1]) + 24):
                fragments.append([])
            fragments[-1].append(run)
        for fragment in fragments:
            # OCR may separate a control's words or the end marker into spans.
            # Keep word boundaries within the current pane's supplied bounds.
            texts = [run["text"] for run in fragment]
            candidates = [*texts, " ".join(texts)]
            if not any(pattern.search(text) or pattern.search(
                    re.sub(r"^[^A-Za-z0-9]+", "", text)) for text in candidates):
                continue
            left = min(run["x"] for run in fragment)
            top = min(run["y"] for run in fragment)
            right = max(run["x"] + run["width"] for run in fragment)
            bottom = max(run["y"] + run["height"] for run in fragment)
            matches.append((round((left + right) / 2), round((top + bottom) / 2)))
            break  # Overlapping OCR spans on the same visual row count once.
    if index < 0 or index >= len(matches):
        raise ValueError("matching transcript row is not visible")
    return matches[index]


if __name__ == "__main__":
    try:
        point = locate(json.load(sys.stdin)["runs"], sys.argv[1],
                       int(sys.argv[2]) if len(sys.argv) > 2 else 0,
                       int(sys.argv[3]) if len(sys.argv) > 3 else 810)
    except ValueError:
        raise SystemExit(1)
    print(f"{point[0]},{point[1]}")
