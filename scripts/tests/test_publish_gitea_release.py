#!/usr/bin/env python3
"""Fixture tests for the Gitea release publisher.

Nothing here touches the network. The parts worth testing are the ones that decide *what* would be
published and *whether* publishing is allowed at all — those are where a wrong answer is silent.

Run from `oxideterm/`:
    python3 scripts/tests/test_publish_gitea_release.py
"""

from __future__ import annotations

import os
import subprocess
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


def run_git(repo: Path, *args: str) -> None:
    subprocess.run(["git", *args], cwd=repo, check=True, capture_output=True)


def plan_for(root: Path, *names: str) -> dict:
    """A plan carrying real files, so the gates see the names they would see in a publish."""
    version = packaging.normalized_version(packaging.raw_release_version())
    return {
        "base_url": "http://example.test",
        "repository": "root/oxideterm",
        "tag": f"v{version}",
        "name": "RayTerm test",
        "channel": "stable",
        "prerelease": False,
        "assets": [str(root / name) for name in names],
    }


class ProvenanceGateTests(unittest.TestCase):
    """Publishing must be reproducible from a commit and pass the release verifier.

    Both gates answer questions whose wrong answer is silent: a release built over uncommitted
    changes looks fine from the download page, and so does an artifact nothing ever checked.
    """

    def test_an_artifact_no_target_claims_is_reported(self):
        name = "RayTerm_2.0.29_mystery_label.zip"
        directory, root = with_artifacts(name)
        with directory, mock.patch.object(publish, "DIST", root):
            problems = publish.verification_problems(plan_for(root, name))
        self.assertEqual(len(problems), 1, problems)
        self.assertIn(f"no release target claims {name}", problems[0])

    def test_incomplete_artifacts_for_a_target_fail_verification(self):
        # One macOS artifact and nothing else must not pass as a published macOS build.
        name = "RayTerm_2.0.29_macos_x64_portable.tar.gz"
        directory, root = with_artifacts(name)
        with directory, mock.patch.object(publish, "DIST", root):
            problems = publish.verification_problems(plan_for(root, name))
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("macos_x64 artifacts failed verification", problems[0])

    def test_ignored_build_output_is_not_an_uncommitted_change(self):
        # Packaging rewrites dist/ and resources/helpers/ on every run. A gate that fires on those
        # would be worked around instead of satisfied, so it has to leave ignored paths alone.
        directory = tempfile.TemporaryDirectory()
        repo = Path(directory.name)
        run_git(repo, "init", "-q")
        (repo / ".gitignore").write_text("dist/\n")
        run_git(repo, "add", ".gitignore")
        run_git(repo, "-c", "user.email=t@example.test", "-c", "user.name=t", "commit", "-qm", "init")
        (repo / "dist").mkdir()
        (repo / "dist" / "artifact.zip").write_bytes(b"artifact")

        with directory, mock.patch.object(packaging, "ROOT_DIR", repo):
            self.assertEqual(publish.uncommitted_paths(), [])
            (repo / "stray.txt").write_text("not build output")
            self.assertEqual(publish.uncommitted_paths(), ["?? stray.txt"])

    def test_publishing_over_an_uncommitted_tree_is_refused(self):
        name = "RayTerm_2.0.29_macos_x64.dmg"
        directory, root = with_artifacts(name)
        stderr = io.StringIO()
        with directory, mock.patch.multiple(
            publish,
            release_plan=lambda: plan_for(root, name),
            verification_problems=lambda plan: [],
            uncommitted_paths=lambda: [" M scripts/release/package_native.py"],
        ), mock.patch.dict(os.environ, {"GITEA_TOKEN": "token"}):
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(stderr):
                self.assertEqual(publish.main(["publish", "--publish"]), 1)
        self.assertIn("uncommitted changes", stderr.getvalue())
        self.assertIn("package_native.py", stderr.getvalue())

    def test_a_dry_run_reports_the_same_problems_but_still_succeeds(self):
        # The dry run exists to show what publishing would do, so it must not hide the refusal.
        name = "RayTerm_2.0.29_macos_x64.dmg"
        directory, root = with_artifacts(name)
        stdout = io.StringIO()
        with directory, mock.patch.multiple(
            publish,
            release_plan=lambda: plan_for(root, name),
            verification_problems=lambda plan: ["macos_x64 artifacts failed verification: boom"],
            uncommitted_paths=lambda: [" M scripts/release/package_native.py"],
        ):
            with contextlib.redirect_stdout(stdout):
                self.assertEqual(publish.main(["publish"]), 0)
        self.assertIn("warning: the working tree has uncommitted changes", stdout.getvalue())
        self.assertIn("warning: macos_x64 artifacts failed verification: boom", stdout.getvalue())

    def test_a_clean_verified_tree_still_publishes(self):
        # The gates are not a wall: clean tree plus verified artifacts has to reach the uploads.
        name = "RayTerm_2.0.29_macos_x64.dmg"
        directory, root = with_artifacts(name)
        uploaded: list[str] = []
        with directory, mock.patch.multiple(
            publish,
            release_plan=lambda: plan_for(root, name),
            verification_problems=lambda plan: [],
            uncommitted_paths=lambda: [],
            release_for_tag=lambda *args: None,
            request=lambda *args, **kwargs: {"id": 7, "assets": []},
        ), mock.patch.object(
            publish,
            "upload_asset",
            lambda base, repo, release_id, path, token: uploaded.append(path.name),
        ), mock.patch.dict(os.environ, {"GITEA_TOKEN": "token"}):
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(publish.main(["publish", "--publish"]), 0)
        self.assertEqual(uploaded, [name])


if __name__ == "__main__":
    unittest.main(verbosity=2)
