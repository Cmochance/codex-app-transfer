#!/usr/bin/env python3
"""Validate that the release notes file for the version exists in the repo."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
NOTES_DIR = ROOT / "release-notes"


def github_error(message: str) -> None:
    print(f"::error::{message}", file=sys.stderr)


def normalize_expected_version(version: str) -> str:
    version = version.strip()
    if not version:
        raise ValueError("expected version is empty")
    if version.startswith("v"):
        raise ValueError(
            f"expected version must not include a leading 'v': {version!r}"
        )
    return version


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Fail release builds when release-notes/v<version>.md is missing "
            "or empty, so the repo copy of every release body stays committed."
        )
    )
    parser.add_argument("expected_version", help="Release version without leading v")
    args = parser.parse_args()

    try:
        expected = normalize_expected_version(args.expected_version)
    except Exception as exc:
        github_error(f"release notes check failed to parse version: {exc}")
        return 1

    notes_file = NOTES_DIR / f"v{expected}.md"
    rel_path = notes_file.relative_to(ROOT)

    if not notes_file.is_file():
        github_error(f"Release notes file missing: {rel_path}")
        github_error(
            "Commit the release notes file in the pre-release PR before "
            "tagging or dispatching, so the repo stays in sync with the "
            "GitHub release body injected via `gh release edit --notes-file`."
        )
        return 1

    if not notes_file.read_text(encoding="utf-8").strip():
        github_error(f"Release notes file is empty: {rel_path}")
        return 1

    print(f"Release notes check passed: {rel_path} exists and is non-empty.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
