#!/usr/bin/env python3
"""Fixture tests for the Gitea release publisher.

Nothing here touches the network. The parts worth testing are the ones that decide *what* would be
published and *whether* publishing is allowed at all — those are where a wrong answer is silent.

Run from `oxideterm/`:
    python3 scripts/tests/test_publish_gitea_release.py
"""

from __future__ import annotations

import json
import os
import subprocess
import urllib.error
import sys
import contextlib
import io
import tempfile
import unittest
from pathlib import Path
from unittest import mock

RELEASE_DIR = Path(__file__).resolve().parents[1] / "release"
sys.path.insert(0, str(RELEASE_DIR))

import build_provenance as provenance  # noqa: E402
import package_native as packaging  # noqa: E402
import publish_gitea_release as publish  # noqa: E402

# Stands in for a commit the repository actually has: these tests never need a real one.
BUILD_COMMIT = "1" * 40


def with_artifacts(*names: str):
    """A temporary dist directory holding the named files."""
    directory = tempfile.TemporaryDirectory()
    root = Path(directory.name)
    for name in names:
        (root / name).write_bytes(b"artifact")
    return directory, root


def stamp_dist(root: Path, **overrides) -> dict:
    """Write the provenance stamp the packaging step leaves beside the artifacts."""
    stamp = {
        "format": provenance.STAMP_FORMAT,
        "version": current_version(),
        "target": "x86_64-apple-darwin",
        "commit": BUILD_COMMIT,
        "branch": "rayterm",
        "dirty": False,
        "built_at": "2026-10-07T00:00:00Z",
    }
    stamp.update(overrides)
    (root / provenance.STAMP_NAME).write_text(json.dumps(stamp), encoding="utf-8")
    return stamp


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


def current_version() -> str:
    """The version this repository is on, so no test has to be edited by a version bump."""
    return packaging.normalized_version(packaging.raw_release_version())


def plan_for(root: Path, *names: str) -> dict:
    """A plan carrying real files, so the gates see the names they would see in a publish."""
    return {
        "base_url": "http://example.test",
        "repository": "root/oxideterm",
        "tag": f"v{current_version()}",
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
        # One macOS artifact and nothing else must not pass as a published macOS build. The version
        # is read from the repository so a bump cannot turn this into a differently-failing test.
        name = f"RayTerm_{current_version()}_macos_x64_portable.tar.gz"
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
            # The path is compared by name: git prints a two-character status code in front of it.
            tracked = publish.uncommitted_paths()
            self.assertEqual(len(tracked), 1)
            self.assertIn("stray.txt", tracked[0])

            # A machine-local setting must not be able to hide a file from this check.
            run_git(repo, "config", "status.showUntrackedFiles", "no")
            hidden = publish.uncommitted_paths()
            self.assertEqual(len(hidden), 1)
            self.assertIn("stray.txt", hidden[0])

    def test_publishing_over_an_uncommitted_tree_is_refused(self):
        name = "RayTerm_2.0.29_macos_x64.dmg"
        directory, root = with_artifacts(name)
        stderr = io.StringIO()
        with directory, mock.patch.multiple(
            publish,
            release_plan=lambda: plan_for(root, name),
            verification_problems=lambda plan: [],
            provenance_problems=lambda plan, stamp: [],
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
            provenance_problems=lambda plan, stamp: [],
            uncommitted_paths=lambda: [" M scripts/release/package_native.py"],
        ):
            with contextlib.redirect_stdout(stdout):
                self.assertEqual(publish.main(["publish"]), 0)
        self.assertIn("warning: the working tree has uncommitted changes", stdout.getvalue())
        self.assertIn("warning: macos_x64 artifacts failed verification: boom", stdout.getvalue())

    def test_a_clean_verified_tree_publishes_and_binds_the_tag_to_the_build_commit(self):
        # The gates are not a wall: a clean tree, verified artifacts and a stamp have to reach the
        # uploads, and the release request has to name the commit the artifacts came from — the
        # server tags its default branch head when that field is missing.
        name = f"RayTerm_{current_version()}_macos_x64.dmg"
        directory, root = with_artifacts(name)
        stamp_dist(root)
        uploaded: list[str] = []
        payloads: list[tuple[str, object]] = []

        def record(method, url, token, payload=None, **kwargs):
            payloads.append((method, payload))
            return {"id": 7, "assets": []}

        with directory, mock.patch.multiple(
            publish,
            release_plan=lambda: plan_for(root, name),
            verification_problems=lambda plan: [],
            provenance_problems=lambda plan, stamp: [],
            uncommitted_paths=lambda: [],
            release_for_tag=lambda *args: None,
            commit_is_published=lambda *args: True,
            request=record,
        ), mock.patch.object(publish, "DIST", root), mock.patch.object(
            publish,
            "upload_asset",
            lambda base, repo, release_id, path, token: uploaded.append(path.name),
        ), mock.patch.dict(os.environ, {"GITEA_TOKEN": "token"}):
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(publish.main(["publish", "--publish"]), 0)
        self.assertEqual(uploaded, [name])
        created = [payload for method, payload in payloads if method == "POST"]
        self.assertEqual(len(created), 1, payloads)
        self.assertEqual(created[0]["target_commitish"], BUILD_COMMIT)


    def test_a_missing_git_is_reported_as_a_publish_error(self):
        # Nothing to inspect the tree with is a refusal, not a crash: the alternative was
        # publishing artifacts whose provenance could not be checked at all.
        with mock.patch.object(provenance.subprocess, "run", side_effect=FileNotFoundError("git")):
            with self.assertRaises(publish.PublishError) as caught:
                publish.uncommitted_paths()
        self.assertIn("git could not be run", str(caught.exception))

    def test_a_gate_that_cannot_run_refuses_cleanly(self):
        # main() has to turn a gate failure into a refusal with a reason. A traceback would hide
        # that publishing was never attempted.
        name = "RayTerm_2.0.29_macos_x64.dmg"
        directory, root = with_artifacts(name)
        stderr = io.StringIO()
        unreachable = mock.Mock(side_effect=publish.PublishError("git could not be run in /x: boom"))
        with directory, mock.patch.multiple(
            publish,
            release_plan=lambda: plan_for(root, name),
            uncommitted_paths=unreachable,
        ), mock.patch.dict(os.environ, {"GITEA_TOKEN": "token"}):
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(stderr):
                self.assertEqual(publish.main(["publish", "--publish"]), 1)
        self.assertIn("error: git could not be run", stderr.getvalue())


    def test_every_target_present_in_dist_is_verified(self):
        # The loop exists so a dist/ holding two platforms (a CI download beside a local build)
        # gets all of them checked; stopping after the first target would pass this silently.
        version = current_version()
        names = [
            f"RayTerm_{version}_macos_x64_portable.tar.gz",
            f"RayTerm_{version}_macos_arm64_portable.tar.gz",
        ]
        directory, root = with_artifacts(*names)
        with directory, mock.patch.object(publish, "DIST", root):
            problems = publish.verification_problems(plan_for(root, *names))
        self.assertEqual(len(problems), 2, problems)
        self.assertIn("macos_x64 artifacts failed verification", problems[0])
        self.assertIn("macos_arm64 artifacts failed verification", problems[1])

    def test_the_packaging_output_directories_are_ignored_here(self):
        # The clean-tree gate only stays usable while packaging output is ignored, and packaging
        # rewrites all three of these on every run. Losing an ignore rule would turn every publish
        # into a refusal; this is the assertion that notices, rather than a report weeks later.
        paths = (
            "dist/",
            "target/",
            "crates/oxideterm-gpui-app/resources/helpers/x86_64-apple-darwin/oxideterm-rdp-helper",
        )
        for path in paths:
            with self.subTest(path=path):
                result = subprocess.run(
                    ["git", "check-ignore", "-q", path],
                    cwd=packaging.ROOT_DIR,
                    capture_output=True,
                )
                self.assertEqual(result.returncode, 0, f"{path} is not ignored")

    def test_many_dirty_paths_are_summarised_rather_than_dumped(self):
        paths = [f" M scripts/release/gate{i}.py" for i in range(7)]
        with mock.patch.multiple(
            publish,
            uncommitted_paths=lambda: paths,
            provenance_problems=lambda plan, stamp: [],
            verification_problems=lambda plan: [],
        ):
            problems = publish.publishability_problems({}, None)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("(+2 more)", problems[0])
        self.assertNotIn("gate6.py", problems[0])


class BuildProvenanceTests(unittest.TestCase):
    """What the stamp says decides whether a publish is allowed, so every field is checked."""

    def problems_for(self, root: Path, name: str, **stamp_overrides) -> list[str]:
        stamp_dist(root, **stamp_overrides)
        with mock.patch.object(publish, "DIST", root):
            return publish.provenance_problems(plan_for(root, name), publish.artifact_stamp())

    def test_artifacts_without_a_stamp_are_refused(self):
        # Without the stamp nothing links the bytes to a commit, which is the whole point.
        name = f"RayTerm_{current_version()}_macos_x64.dmg"
        directory, root = with_artifacts(name)
        with directory, mock.patch.object(publish, "DIST", root):
            problems = publish.provenance_problems(plan_for(root, name), None)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn(provenance.STAMP_NAME, problems[0])

    def test_artifacts_built_over_a_dirty_tree_are_refused(self):
        name = f"RayTerm_{current_version()}_macos_x64.dmg"
        directory, root = with_artifacts(name)
        with directory:
            problems = self.problems_for(root, name, dirty=True)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("uncommitted changes", problems[0])

    def test_a_stamp_that_could_not_tell_whether_the_tree_was_dirty_is_refused(self):
        # None is "the build could not tell", which is not the same as clean.
        name = f"RayTerm_{current_version()}_macos_x64.dmg"
        directory, root = with_artifacts(name)
        with directory:
            problems = self.problems_for(root, name, dirty=None)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("uncommitted changes", problems[0])

    def test_a_stamp_without_a_commit_is_refused(self):
        name = f"RayTerm_{current_version()}_macos_x64.dmg"
        directory, root = with_artifacts(name)
        with directory:
            problems = self.problems_for(root, name, commit=None)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("records no commit", problems[0])

    def test_a_stamp_written_for_another_version_is_refused(self):
        # A stale stamp would bind a new version's tag to an old build's commit.
        name = f"RayTerm_{current_version()}_macos_x64.dmg"
        directory, root = with_artifacts(name)
        with directory:
            problems = self.problems_for(root, name, version="0.0.1")
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("0.0.1", problems[0])

    def test_an_unreadable_stamp_refuses_cleanly(self):
        # The stamp is read before the gate runs, so an unreadable one has to refuse there.
        name = f"RayTerm_{current_version()}_macos_x64.dmg"
        directory, root = with_artifacts(name)
        (root / provenance.STAMP_NAME).write_text("{not json", encoding="utf-8")
        with directory, mock.patch.object(publish, "DIST", root):
            with self.assertRaises(publish.PublishError) as caught:
                publish.artifact_stamp()
        self.assertIn(provenance.STAMP_NAME, str(caught.exception))

    def test_a_stamp_that_is_not_even_utf8_refuses_cleanly(self):
        # A truncated write or a stray binary of the same name must not end in a traceback.
        name = f"RayTerm_{current_version()}_macos_x64.dmg"
        directory, root = with_artifacts(name)
        (root / provenance.STAMP_NAME).write_bytes(b"\xff\xfe\x00 not utf-8")
        with directory, mock.patch.object(publish, "DIST", root):
            with self.assertRaises(publish.PublishError) as caught:
                publish.artifact_stamp()
        self.assertIn(provenance.STAMP_NAME, str(caught.exception))

    def test_a_dirty_stamp_refuses_through_main_not_only_when_asked_directly(self):
        # A gate main() does not consult protects nothing: the other two are mocked clean here, so
        # only the provenance gate can produce the refusal.
        name = f"RayTerm_{current_version()}_macos_x64.dmg"
        directory, root = with_artifacts(name)
        stamp_dist(root, dirty=True)
        stderr = io.StringIO()
        with directory, mock.patch.object(publish, "DIST", root), mock.patch.multiple(
            publish,
            release_plan=lambda: plan_for(root, name),
            verification_problems=lambda plan: [],
            uncommitted_paths=lambda: [],
            publish_release=lambda *args: None,
        ), mock.patch.dict(os.environ, {"GITEA_TOKEN": "token"}):
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(stderr):
                self.assertEqual(publish.main(["publish", "--publish"]), 1)
        self.assertIn("built over uncommitted changes", stderr.getvalue())

    def test_a_failure_while_publishing_is_reported_rather_than_tracebacked(self):
        # Everything after the gates talks to the server. A refused upload has to say so.
        name = f"RayTerm_{current_version()}_macos_x64.dmg"
        directory, root = with_artifacts(name)
        stamp_dist(root)
        stderr = io.StringIO()
        with directory, mock.patch.object(publish, "DIST", root), mock.patch.multiple(
            publish,
            release_plan=lambda: plan_for(root, name),
            verification_problems=lambda plan: [],
            uncommitted_paths=lambda: [],
            commit_is_published=lambda *args: True,
            tag_commit=lambda *args: None,
            release_for_tag=lambda *args: {"id": 3, "assets": []},
            upload_asset=mock.Mock(side_effect=publish.PublishError("uploading x failed: HTTP 500")),
        ), mock.patch.dict(os.environ, {"GITEA_TOKEN": "token"}):
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(stderr):
                self.assertEqual(publish.main(["publish", "--publish"]), 1)
        self.assertIn("uploading x failed", stderr.getvalue())

    def test_creating_a_release_over_an_existing_tag_says_where_that_tag_points(self):
        # A tag can already exist without a release (pushed by hand, or the release was deleted),
        # and this API cannot move it: the create path has to warn just like the append path.
        name = f"RayTerm_{current_version()}_macos_x64.dmg"
        directory, root = with_artifacts(name)
        stamp_dist(root)
        stderr = io.StringIO()
        created: list[object] = []

        def record(method, url, token, payload=None, **kwargs):
            created.append(payload)
            return {"id": 5, "assets": []}

        with directory, mock.patch.object(publish, "DIST", root), mock.patch.multiple(
            publish,
            release_plan=lambda: plan_for(root, name),
            verification_problems=lambda plan: [],
            uncommitted_paths=lambda: [],
            commit_is_published=lambda *args: True,
            tag_commit=lambda *args: "3" * 40,
            release_for_tag=lambda *args: None,
            request=record,
            upload_asset=lambda *args: None,
        ), mock.patch.dict(os.environ, {"GITEA_TOKEN": "token"}):
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(stderr):
                self.assertEqual(publish.main(["publish", "--publish"]), 0)
        self.assertEqual(created[0]["target_commitish"], BUILD_COMMIT)
        self.assertIn("already points at", stderr.getvalue())
        self.assertIn("3" * 9, stderr.getvalue())

    def test_a_tag_that_cannot_be_read_warns_but_does_not_block(self):
        # Nothing depends on this read except a warning, so it must not fail a verified publish —
        # but it must not vanish either, which is how a misplaced tag goes unnoticed.
        name = f"RayTerm_{current_version()}_macos_x64.dmg"
        directory, root = with_artifacts(name)
        stamp_dist(root)
        stderr = io.StringIO()
        with directory, mock.patch.object(publish, "DIST", root), mock.patch.multiple(
            publish,
            release_plan=lambda: plan_for(root, name),
            verification_problems=lambda plan: [],
            uncommitted_paths=lambda: [],
            commit_is_published=lambda *args: True,
            tag_commit=mock.Mock(side_effect=publish.PublishError("GET tags/... failed: HTTP 500")),
            release_for_tag=lambda *args: {"id": 3, "assets": []},
            upload_asset=lambda *args: None,
        ), mock.patch.dict(os.environ, {"GITEA_TOKEN": "token"}):
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(stderr):
                self.assertEqual(publish.main(["publish", "--publish"]), 0)
        self.assertIn("could not read tag", stderr.getvalue())

    def test_a_commit_the_server_does_not_have_is_refused(self):
        # Tagging a commit the server lacks is how the tag ends up on the default branch instead.
        name = f"RayTerm_{current_version()}_macos_x64.dmg"
        directory, root = with_artifacts(name)
        stamp_dist(root)
        stderr = io.StringIO()
        with directory, mock.patch.object(publish, "DIST", root), mock.patch.multiple(
            publish,
            release_plan=lambda: plan_for(root, name),
            verification_problems=lambda plan: [],
            uncommitted_paths=lambda: [],
            commit_is_published=lambda *args: False,
        ), mock.patch.dict(os.environ, {"GITEA_TOKEN": "token"}):
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(stderr):
                self.assertEqual(publish.main(["publish", "--publish"]), 1)
        self.assertIn("not on root/oxideterm yet", stderr.getvalue())

    def test_a_build_commit_that_is_not_head_is_a_warning_not_a_refusal(self):
        # The tag is bound to the stamp's commit, so an older build is publishable; the operator
        # still deserves to be told that is what happened.
        with mock.patch.object(provenance, "head_commit", lambda root: "2" * 40):
            warnings = publish.provenance_warnings({"commit": "1" * 40})
        self.assertEqual(len(warnings), 1, warnings)
        self.assertIn("1" * 9, warnings[0])
        self.assertIn("2" * 9, warnings[0])

    def test_appending_to_a_tag_that_points_elsewhere_says_so(self):
        # This API cannot move a tag, so the only honest thing left is to record the mismatch.
        name = f"RayTerm_{current_version()}_macos_x64.dmg"
        directory, root = with_artifacts(name)
        stamp_dist(root)
        stderr = io.StringIO()
        with directory, mock.patch.object(publish, "DIST", root), mock.patch.multiple(
            publish,
            release_plan=lambda: plan_for(root, name),
            verification_problems=lambda plan: [],
            uncommitted_paths=lambda: [],
            commit_is_published=lambda *args: True,
            release_for_tag=lambda *args: {"id": 9, "assets": []},
            tag_commit=lambda *args: "3" * 40,
            upload_asset=lambda *args: None,
        ), mock.patch.dict(os.environ, {"GITEA_TOKEN": "token"}):
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(stderr):
                self.assertEqual(publish.main(["publish", "--publish"]), 0)
        self.assertIn("already points at", stderr.getvalue())
        self.assertIn("3" * 9, stderr.getvalue())


    def test_the_commit_check_asks_a_route_this_gitea_serves(self):
        # The route is part of the contract with the server: the deployed Gitea has no
        # /commits/{sha}, and the 404 it answered for every commit would have refused every
        # publish. A mocked status cannot catch that, so the requested URL is asserted.
        asked: list[str] = []

        class Response:
            def __enter__(self):
                return self

            def __exit__(self, *args):
                return False

            def read(self):
                return b""
        def fake_urlopen(request, timeout=None):
            asked.append(request.full_url)
            return Response()

        commit = "a" * 40
        with mock.patch.object(publish.urllib.request, "urlopen", fake_urlopen):
            self.assertTrue(
                publish.commit_is_published("http://gitea.test", "root/oxideterm", commit, "token")
            )
        self.assertEqual(
            asked,
            [f"http://gitea.test/api/v1/repos/root/oxideterm/git/commits/{commit}"],
        )

    def test_a_commit_the_server_lacks_is_not_found_rather_than_an_error(self):
        missing = urllib.error.HTTPError("http://gitea.test", 404, "Not Found", {}, io.BytesIO(b""))
        with mock.patch.object(publish.urllib.request, "urlopen", side_effect=missing):
            self.assertFalse(
                publish.commit_is_published("http://gitea.test", "root/oxideterm", "a" * 40, "token")
            )

    def test_a_broken_answer_while_checking_the_commit_is_a_publish_error(self):
        # Anything other than 404 means the question could not be answered, and an unanswered
        # question must not read as "the commit is there".
        broken = urllib.error.HTTPError(
            "http://gitea.test", 500, "Server Error", {}, io.BytesIO(b"boom")
        )
        with mock.patch.object(publish.urllib.request, "urlopen", side_effect=broken):
            with self.assertRaises(publish.PublishError) as caught:
                publish.commit_is_published("http://gitea.test", "root/oxideterm", "a" * 40, "token")
        self.assertIn("HTTP 500", str(caught.exception))


if __name__ == "__main__":
    unittest.main(verbosity=2)
