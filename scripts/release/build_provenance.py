#!/usr/bin/env python3
"""Record which commit the packaging step ran on, and read that record back.

The packaging step copies resources straight out of the working tree, so a `dist/` directory
carries no clue about the code it came from. This module writes that clue beside the artifacts
(`dist/.BUILD-PROVENANCE.json`) and lets the publisher read it. Both sides share the git
questions here so there is one implementation of "is this tree clean" rather than two that
can disagree.
"""

from __future__ import annotations

import json
import subprocess
from datetime import datetime, timezone
from pathlib import Path

STAMP_NAME = ".BUILD-PROVENANCE.json"
STAMP_FORMAT = 1


class ProvenanceError(RuntimeError):
    """The working tree could not be inspected at all."""


def _git(root: Path, *args: str) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            ["git", *args],
            cwd=root,
            capture_output=True,
            text=True,
        )
    except OSError as error:
        raise ProvenanceError(f"git could not be run in {root}: {error}") from error


def uncommitted_paths(root: Path) -> list[str]:
    """Working tree entries that no commit holds yet.

    Ignored build output stays out of this list: packaging rewrites `dist/`,
    `resources/cli-bin/` and `resources/helpers/` on every run, and a cleanliness check that
    fired on those would be worked around instead of satisfied. `--untracked-files=all` pins
    the query so a machine-local `status.showUntrackedFiles=no` cannot hide new files.
    """
    result = _git(root, "status", "--porcelain", "--untracked-files=all")
    if result.returncode != 0:
        raise ProvenanceError(f"git status failed in {root}: {result.stderr.strip()}")
    return [line for line in result.stdout.splitlines() if line.strip()]


def head_commit(root: Path) -> str | None:
    """The commit the tree is on, or None when there is no repository to ask."""
    result = _git(root, "rev-parse", "HEAD")
    if result.returncode != 0:
        return None
    return result.stdout.strip() or None


def current_branch(root: Path) -> str | None:
    """The branch name, or None on a detached HEAD (where the name would be a lie)."""
    result = _git(root, "rev-parse", "--abbrev-ref", "HEAD")
    if result.returncode != 0:
        return None
    name = result.stdout.strip()
    return name if name and name != "HEAD" else None


def write_stamp(
    dist: Path,
    *,
    version: str,
    target: str,
    root: Path,
    built_at: str | None = None,
) -> Path:
    """Write the provenance stamp for the artifacts just produced in `dist`.

    Everything is recorded as found rather than enforced here: a build over a dirty tree still
    succeeds (building locally all day is normal), and the publisher is where that becomes a
    refusal. A tree that cannot be inspected records nulls, which the publisher also refuses.
    """
    # Every question is answered defensively: a machine without git, or a directory that is not a
    # repository, still has to produce a stamp — with nulls, which the publisher refuses. Failing
    # the build here would take that decision away from the side that owns it.
    try:
        dirty: bool | None = bool(uncommitted_paths(root))
        commit: str | None = head_commit(root)
        branch: str | None = current_branch(root)
    except ProvenanceError:
        dirty, commit, branch = None, None, None
    stamp = {
        "format": STAMP_FORMAT,
        "version": version,
        "target": target,
        "commit": commit,
        "branch": branch,
        "dirty": dirty,
        "built_at": built_at or datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    }
    path = dist / STAMP_NAME
    path.write_text(json.dumps(stamp, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return path


def read_stamp(dist: Path) -> dict | None:
    """The stamp in `dist`, or None when the packaging step left none."""
    path = dist / STAMP_NAME
    if not path.is_file():
        return None
    try:
        stamp = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ProvenanceError(f"{path} could not be read: {error}") from error
    if not isinstance(stamp, dict):
        raise ProvenanceError(f"{path} does not hold an object")
    return stamp
