#!/usr/bin/env python3
"""Fixture tests for the Gitea release publisher.

Nothing here touches the network. The parts worth testing are the ones that decide *what* would be
published and *whether* publishing is allowed at all — those are where a wrong answer is silent.

Run from `oxideterm/`:
    python3 scripts/tests/test_publish_gitea_release.py
"""

from __future__ import annotations

import os
import sys
import contextlib
import io
import tempfile
import unittest
from pathlib import Path
from unittest import mock

RELEASE_DIR = Path(__file__).resolve().parents[1] / "release"
sys.path.insert(0, str(RELEASE_DIR))

import package_native as packaging  # noqa: E402
import publish_gitea_release as publish  # noqa: E402


def with_artifacts(*names: str):
    """A temporary dist directory holding the named files."""
    directory = tempfile.TemporaryDirectory()
    root = Path(directory.name)
    for name in names:
        (root / name).write_bytes(b"artifact")
    return directory, root


class PlanTests(unittest.TestCase):
    def test_no_artifacts_is_an_error_rather_than_an_empty_release(self):
        # A release with no assets is worse than no release: it claims a version is available
        # while offering nothing to download.
        directory, root = with_artifacts()
        with directory, mock.patch.object(publish, "DIST", root):
            with self.assertRaises(publish.PublishError) as caught:
                publish.release_plan()
        self.assertIn("no artifacts", str(caught.exception))

    def test_the_plan_names_the_version_the_repository_and_every_artifact(self):
        directory, root = with_artifacts(
            "RayTerm_2.0.29_aarch64-apple-darwin.app.zip",
            "RayTerm_2.0.29_aarch64-apple-darwin_portable.zip",
        )
        version = packaging.normalized_version(packaging.raw_release_version())
        with directory, mock.patch.object(publish, "DIST", root):
            plan = publish.release_plan()

        self.assertEqual(plan["tag"], f"v{version}")
        self.assertEqual(plan["repository"], "root/oxideterm")
        self.assertEqual(plan["base_url"], plan["base_url"].rstrip("/"))
        self.assertEqual(len(plan["assets"]), 2)
        # The name users see comes from the branding config, not from this script.
        self.assertTrue(plan["name"].startswith("RayTerm "))

    def test_a_dot_file_is_not_published(self):
        # Staging files are written beside the artifacts and must not be offered as downloads.
        directory, root = with_artifacts(
            "RayTerm_2.0.29_aarch64-apple-darwin.app.zip",
            ".RayTerm_2.0.29_aarch64-apple-darwin-notarization.zip",
        )
        with directory, mock.patch.object(publish, "DIST", root):
            plan = publish.release_plan()
        self.assertEqual(
            [Path(asset).name for asset in plan["assets"]],
            ["RayTerm_2.0.29_aarch64-apple-darwin.app.zip"],
        )

    def test_a_plain_version_is_not_marked_prerelease(self):
        directory, root = with_artifacts("a.zip")
        with directory, mock.patch.object(publish, "DIST", root), mock.patch.object(
            packaging, "raw_release_version", lambda: "2.0.29"
        ):
            plan = publish.release_plan()
        self.assertFalse(plan["prerelease"])

    def test_a_dashed_version_is_marked_prerelease(self):
        # A pre-release that is published as a final release is the one mistake here that users
        # notice only after installing it.
        directory, root = with_artifacts("a.zip")
        with directory, mock.patch.object(publish, "DIST", root), mock.patch.object(
            packaging, "raw_release_version", lambda: "2.1.0-rc.1"
        ):
            plan = publish.release_plan()
        self.assertTrue(plan["prerelease"])
        self.assertEqual(plan["tag"], "v2.1.0-rc.1")


class GateTests(unittest.TestCase):
    def test_publishing_without_a_token_fails_and_says_why(self):
        directory, root = with_artifacts("a.zip")
        with directory, mock.patch.object(publish, "DIST", root), mock.patch.dict(
            os.environ, {}, clear=False
        ):
            os.environ.pop("GITEA_TOKEN", None)
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(publish.main(["publish", "--publish"]), 1)

    def test_a_dry_run_needs_no_token(self):
        directory, root = with_artifacts("a.zip")
        with directory, mock.patch.object(publish, "DIST", root):
            os.environ.pop("GITEA_TOKEN", None)
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(publish.main(["publish"]), 0)

    def test_a_non_release_channel_is_refused(self):
        # A throwaway build made while developing must not become a release by accident.
        directory, root = with_artifacts("a.zip")
        plan = {
            "base_url": "http://example.test",
            "repository": "root/x",
            "tag": "v1",
            "name": "X 1",
            "channel": "development",
            "prerelease": True,
            "assets": [],
        }
        with directory, mock.patch.object(publish, "DIST", root), mock.patch.object(
            publish, "release_plan", lambda: plan
        ):
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(publish.main(["publish"]), 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
