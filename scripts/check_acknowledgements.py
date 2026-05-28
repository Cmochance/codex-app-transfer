#!/usr/bin/env python3
"""Enforce that each README acknowledgements *overview* entry stays a terse tag.

The `## 致谢` / `## Acknowledgements` list is an overview only — the full borrowing
form / license / itemized list / file:line lives in ACKNOWLEDGEMENTS.md. Written
guidelines kept getting ignored (new entries grew back into paragraphs), so the
length budget is enforced here and wired into CI.

Rule: the description after the ` — ` separator must not exceed a per-file budget,
measured in Unicode code points ("字") — CJK, Latin, digits, punctuation, backticks
and spaces each count as 1. Only the project-acknowledgement bullets are checked; the
`### 社区贡献者` / `### Community contributors` subsection (PR links) is excluded.

Run: python3 scripts/check_acknowledgements.py
Exits non-zero with ::error:: annotations on any violation.
"""
from __future__ import annotations

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

# README path -> max code points allowed in an overview entry's description. The
# Chinese overview holds the project's ≤20 字 rule; the English mirror gets a looser
# budget because equivalent phrasing runs longer per concept.
LIMITS = {
    "README.md": 20,
    "README.en.md": 40,
}

SECTION_HEADINGS = ("## 致谢", "## Acknowledgements")
CONTRIB_SUBHEADINGS = ("### 社区贡献者", "### Community contributors")
SEP = " — "  # space, em dash (U+2014), space


def github_error(message: str) -> None:
    print(f"::error::{message}", file=sys.stderr)


def overview_entries(text: str):
    """Yield (line_no, description_or_None) for each acknowledgement overview bullet.

    description is None when the bullet is missing the ` — ` separator (malformed).
    """
    in_section = False
    for line_no, raw in enumerate(text.splitlines(), start=1):
        line = raw.rstrip()
        if line in SECTION_HEADINGS:
            in_section = True
            continue
        if not in_section:
            continue
        # End of the overview list: contributors subsection or the next H2.
        if line.startswith(CONTRIB_SUBHEADINGS) or line.startswith("## "):
            break
        if not line.startswith("- ["):
            continue
        if SEP not in line:
            yield line_no, None
            continue
        yield line_no, line.split(SEP, 1)[1].strip()


def main() -> int:
    violations = 0
    checked = 0
    for name, limit in LIMITS.items():
        path = ROOT / name
        if not path.exists():
            github_error(f"{name}: file not found")
            violations += 1
            continue

        entries = list(overview_entries(path.read_text(encoding="utf-8")))
        if not entries:
            github_error(
                f"{name}: no acknowledgements overview entries found — did the "
                "'## 致谢' / '## Acknowledgements' heading or list format change?"
            )
            violations += 1
            continue

        for line_no, desc in entries:
            checked += 1
            if desc is None:
                github_error(
                    f"{name}:{line_no}: acknowledgement entry missing the "
                    f"'{SEP.strip()}' description separator"
                )
                violations += 1
                continue
            length = len(desc)
            if length > limit:
                github_error(
                    f"{name}:{line_no}: description is {length} 字 (limit {limit}) — "
                    f"shorten the overview tag, move detail to ACKNOWLEDGEMENTS.md: "
                    f"{desc!r}"
                )
                violations += 1

    if violations:
        budgets = ", ".join(f"{k}≤{v}" for k, v in LIMITS.items())
        github_error(
            f"acknowledgements length check failed: {violations} violation(s) "
            f"across {checked} entry(ies). Budgets: {budgets}."
        )
        return 1

    budgets = ", ".join(f"{k}≤{v}" for k, v in LIMITS.items())
    print(
        f"acknowledgements length check passed: {checked} entry(ies) within budget "
        f"({budgets})."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
