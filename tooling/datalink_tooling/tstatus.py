#!/usr/bin/env python3
"""Scan lessons-learned.md for T-* item lifecycle markers and print
the open + closed lists.

DB-agnostic: the feedback format (the `(T-N new)` / `(T-N closed)`
markers and the lessons-learned.md convention) is shared across repos;
only the doc path is per-repo (config feedback.lessons_doc).

Markers recognized (case-insensitive, anywhere in a line):
  (T-N new)            opened
  (T-N closed)         closed (any sub-clause: "closed inline",
                        "closed in same doc", "silently closed", ...)

The TITLE for a T-N is the surrounding markdown section title.

Usage:
  t-status.py [--config PATH] [all|open|closed]
"""
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

from . import dlconfig  # noqa: E402

T_OPEN = re.compile(r"\(T-(\d+)\s+new\)", re.IGNORECASE)
T_CLOSED = re.compile(r"\(T-(\d+)[^)]*closed[^)]*\)", re.IGNORECASE)
SECTION = re.compile(r"^###\s+(.*?)\s*$")


def scan(doc: Path) -> tuple[dict[int, tuple[str, str]], dict[int, tuple[str, str]]]:
    """Return (open, closed) dicts: id -> (section_title, marker_line)."""
    text = doc.read_text()
    section_title = "(top)"
    opens: dict[int, tuple[str, str]] = {}
    closes: dict[int, tuple[str, str]] = {}
    for line in text.splitlines():
        if (m := SECTION.match(line)):
            section_title = m.group(1)
            continue
        # First-match-wins: the ORIGINAL open / close marker is the
        # canonical one; later mentions (including doc quotes of the
        # regex patterns themselves) shouldn't overwrite the section.
        for m in T_OPEN.finditer(line):
            n = int(m.group(1))
            opens.setdefault(n, (section_title, line.strip()))
        for m in T_CLOSED.finditer(line):
            n = int(m.group(1))
            closes.setdefault(n, (section_title, line.strip()))
    return opens, closes


def main(config: str | None = None, argv=None) -> None:
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    dlconfig.add_config_arg(p, default=config)
    p.add_argument("which", nargs="?", default="all",
                   choices=("all", "open", "closed"))
    args = p.parse_args(argv)

    cfg = dlconfig.load(args.config)
    doc_rel = cfg.get("feedback", "lessons_doc", default="tooling/lessons-learned.md")
    doc = cfg.path(doc_rel)
    if not doc.is_file():
        sys.exit(f"error: lessons doc not found: {doc}")

    opens, closes = scan(doc)
    # An item is OPEN if it has a new-marker AND no closed-marker.
    open_ids = sorted(set(opens) - set(closes))
    closed_ids = sorted(set(closes))

    if args.which in ("all", "open"):
        print(f"Open ({len(open_ids)}):")
        for n in open_ids:
            section, _ = opens[n]
            print(f"  T-{n:<3}  {section}")
        if args.which == "all":
            print()

    if args.which in ("all", "closed"):
        print(f"Closed ({len(closed_ids)}):")
        for n in closed_ids:
            section, _ = closes[n]
            print(f"  T-{n:<3}  {section}")


if __name__ == "__main__":
    main()
