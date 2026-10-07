#!/usr/bin/env python3
"""Publish the packaged artifacts as a Gitea release.

The packaging step writes `dist/`; this is the step that makes them downloadable. They are
separate on purpose: packaging runs many times locally, publishing happens once per version and
is not repeatable — a tag that already carries assets cannot be republished cleanly.

Run from `oxideterm/`:
    python3 scripts/release/publish_gitea_release.py             # dry run, prints the plan
    python3 scripts/release/publish_gitea_release.py --publish   # creates the release

The token is read from GITEA_TOKEN. It is never taken from an argument, a file or the config: an
argument lands in the shell history and the process table, and a file lands in the repository.
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "custom-branding"))

import build_provenance as provenance  # noqa: E402
import package_native as packaging  # noqa: E402
import verify_native_package as verifying  # noqa: E402
from branding import load as load_branding  # noqa: E402

DIST = packaging.DIST_DIR
# Internal deploys also carry throwaway builds made while developing; they should not become
# releases by accident.
RELEASABLE_CHANNELS = ("stable", "preview", "gpui-preview", "native-preview")


class PublishError(RuntimeError):
    """The release cannot be published as asked."""


def release_plan() -> dict:
    """What would be published, derived from the same inputs packaging used."""
    raw_version = packaging.raw_release_version()
    version = packaging.normalized_version(raw_version)
    identity = packaging.release_identity(raw_version, version)
    branding = load_branding()
    distribution = branding["distribution"]

    assets = sorted(path for path in DIST.glob("*") if path.is_file() and not path.name.startswith("."))
    if not assets:
        raise PublishError(f"no artifacts in {DIST}; run the packaging step first")

    return {
        "base_url": distribution["giteaUrl"].rstrip("/"),
        "repository": distribution["repository"],
        "tag": f"v{version}",
        "name": f"{identity.app_name} {version}",
        "channel": identity.channel,
        "prerelease": identity.channel != "stable",
        "assets": [str(path) for path in assets],
    }


def uncommitted_paths() -> list[str]:
    """Working tree entries that no commit holds yet.

    Packaging copies resources straight out of the working tree, so artifacts built over
    uncommitted changes cannot be rebuilt from the repository afterwards — that is how the
    v2.0.29 release ended up with no commit behind it. The shared module owns the git question so
    that the stamp the packaging step writes and this check answer it the same way.
    """
    try:
        return provenance.uncommitted_paths(packaging.ROOT_DIR)
    except provenance.ProvenanceError as error:
        raise PublishError(str(error)) from error


def artifact_stamp() -> dict | None:
    """The stamp the packaging step left in dist/, or None when it left none."""
    try:
        return provenance.read_stamp(DIST)
    except provenance.ProvenanceError as error:
        raise PublishError(str(error)) from error


def verification_problems(plan: dict) -> list[str]:
    """What the release verifier reports about the artifacts this plan would publish.

    The verifier works per target, so every target present in dist/ gets its own run. Packaging
    clears dist/ at the start of each run, so several targets appear here only when artifacts are
    staged together (a CI download beside a local build). An artifact no target claims is a problem
    too: nothing would have checked it.
    """
    version = verifying.normalized_version(plan["tag"])
    names = {Path(asset).name for asset in plan["assets"]}
    claimed: set[str] = set()
    problems: list[str] = []
    for target in verifying.TARGET_LABELS:
        expected = verifying.expected_artifact_names(target, version)
        if not names & expected:
            continue
        claimed |= names & expected
        try:
            verifying.verify_release(DIST, target, version)
        except Exception as error:  # the verifier reports failures through several exception types
            problems.append(f"{verifying.target_label(target)} artifacts failed verification: {error}")
    unclaimed = sorted(names - claimed)
    if unclaimed:
        problems.append(f"no release target claims {', '.join(unclaimed)}")
    return problems


def provenance_problems(plan: dict, stamp: dict | None) -> list[str]:
    """What the build provenance stamp says about the artifacts, as refusal reasons.

    The clean-tree check below only knows the tree as it is right now; the stamp records the tree
    the artifacts were actually built on, which is what a release has to be reproducible from.
    """
    if stamp is None:
        return [
            f"{DIST} holds no {provenance.STAMP_NAME}: these artifacts did not come from the "
            "packaging step of this checkout, or they predate provenance stamps"
        ]
    problems: list[str] = []
    if stamp.get("format") != provenance.STAMP_FORMAT:
        problems.append(
            f"the provenance stamp is format {stamp.get('format')!r}, expected {provenance.STAMP_FORMAT}"
        )
    if stamp.get("dirty") is not False:
        # None means the build could not tell, which is not the same as clean.
        problems.append("the artifacts were built over uncommitted changes, so no commit can rebuild them")
    if not stamp.get("commit"):
        problems.append("the provenance stamp records no commit")
    version = verifying.normalized_version(plan["tag"])
    if stamp.get("version") != version:
        problems.append(
            f"the stamp was written for version {stamp.get('version')!r}, but this release is {version!r}"
        )
    return problems


def provenance_warnings(stamp: dict | None) -> list[str]:
    """What is worth saying out loud about the provenance without blocking the publish."""
    commit = (stamp or {}).get("commit")
    head = provenance.head_commit(packaging.ROOT_DIR)
    if commit and head and commit != head:
        return [
            f"the artifacts were built from {commit[:9]}, not the current HEAD {head[:9]}: "
            "the tag will point at the build commit"
        ]
    return []


def publishability_problems(plan: dict, stamp: dict | None) -> list[str]:
    """Every reason publishing right now would be a mistake, as readable one-liners.

    `stamp` is the one the caller read, so a publish tags the commit that was actually checked.
    """
    problems: list[str] = []
    dirty = uncommitted_paths()
    if dirty:
        shown = ", ".join(dirty[:5])
        hidden = "" if len(dirty) <= 5 else f" (+{len(dirty) - 5} more)"
        problems.append(f"the working tree has uncommitted changes: {shown}{hidden}")
    problems.extend(provenance_problems(plan, stamp))
    problems.extend(verification_problems(plan))
    return problems


def request(
    method: str,
    url: str,
    token: str,
    payload: object | None = None,
    *,
    missing_ok: bool = False,
) -> object:
    data = None
    headers = {"Authorization": f"token {token}", "Accept": "application/json"}
    if payload is not None:
        data = json.dumps(payload).encode("utf-8")
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=60) as response:
            body = response.read()
    except urllib.error.HTTPError as error:
        if missing_ok and error.code == 404:
            # "There is no such thing" is an answer some callers ask for, not a failure.
            return None
        detail = error.read().decode("utf-8", "replace")[:400]
        raise PublishError(f"{method} {url} failed: HTTP {error.code} {detail}") from error
    except urllib.error.URLError as error:
        raise PublishError(f"{method} {url} failed: {error.reason}") from error
    if not body:
        return None
    try:
        return json.loads(body)
    except json.JSONDecodeError:
        return None


def commit_is_published(base: str, repository: str, commit: str, token: str) -> bool:
    """Whether the server knows this commit, since a tag can only point at a commit it has.

    A 404 is an answer rather than an error: publishing before pushing would tag whatever the
    server's default branch happens to hold, which is the mistake this check exists for.
    """
    # Gitea 1.24 (the deployed version) serves a commit at /git/commits/{sha}; it has no
    # /commits/{sha} route, and asking for that one answered 404 for every commit — which would
    # have refused every publish with "push it first".
    url = f"{base}/api/v1/repos/{repository}/git/commits/{commit}"
    req = urllib.request.Request(
        url,
        headers={"Authorization": f"token {token}", "Accept": "application/json"},
        method="GET",
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as response:
            response.read()
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return False
        detail = error.read().decode("utf-8", "replace")[:400]
        raise PublishError(f"checking {commit[:9]} failed: HTTP {error.code} {detail}") from error
    except urllib.error.URLError as error:
        raise PublishError(f"checking {commit[:9]} failed: {error.reason}") from error
    return True


def tag_commit(base: str, repository: str, tag: str, token: str) -> str | None:
    """The commit an existing tag points at, or None when there is no such tag.

    Only "there is no such tag" answers None; any other failure is raised. This read feeds a
    warning, and a warning that quietly disappears is how a tag pointing at another commit goes
    unnoticed — the caller decides whether to say so or to stop.
    """
    payload = request("GET", f"{base}/api/v1/repos/{repository}/tags/{tag}", token, missing_ok=True)
    if isinstance(payload, dict):
        commit = payload.get("commit")
        if isinstance(commit, dict) and isinstance(commit.get("sha"), str):
            return commit["sha"]
    return None


def release_for_tag(base_url: str, repository: str, tag: str, token: str) -> dict | None:
    """The release already carrying this tag, if any.

    Publishing is per platform but a tag is per version, so a second platform must land on the
    release the first one created. Creating another release for the same tag would split one
    version across several downloads pages and leave the tag pointing at whichever came first.
    """
    releases = request("GET", f"{base_url}/api/v1/repos/{repository}/releases?limit=50", token)
    if not isinstance(releases, list):
        return None
    for release in releases:
        if isinstance(release, dict) and release.get("tag_name") == tag:
            return release
    return None


def upload_asset(base_url: str, repository: str, release_id: int, path: Path, token: str) -> None:
    """Uploads one artifact.

    Streamed from disk rather than read into memory: a macOS bundle is over a hundred megabytes,
    and holding that to satisfy a request body is memory the process does not need to spend.
    """
    url = f"{base_url}/api/v1/repos/{repository}/releases/{release_id}/assets?name={path.name}"
    with path.open("rb") as handle:
        request_body = handle.read()
    headers = {
        "Authorization": f"token {token}",
        "Content-Type": "application/octet-stream",
    }
    req = urllib.request.Request(url, data=request_body, headers=headers, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=1800) as response:
            response.read()
    except urllib.error.HTTPError as error:
        detail = error.read().decode("utf-8", "replace")[:400]
        raise PublishError(f"uploading {path.name} failed: HTTP {error.code} {detail}") from error


def publish_release(plan: dict, stamp: dict | None, token: str) -> None:
    """Create or extend the release and upload the artifacts, or explain why not.

    Split out of main() so every server-side failure leaves through one place. `stamp` is the one
    the gate checked, so the commit tagged here is the commit that was verified.
    """
    base = plan["base_url"]
    repository = plan["repository"]
    commit = (stamp or {}).get("commit")
    if not commit:
        # The gate refuses a stamp without a commit, so reaching this means the caller skipped it.
        raise PublishError("the artifacts carry no build commit to bind the tag to")
    if not commit_is_published(base, repository, commit, token):
        raise PublishError(
            f"{commit[:9]} is not on {repository} yet. Push it first: a tag can only point at a "
            "commit the server has, and without it the tag lands on the default branch"
        )

    # Read before the release is created or extended: this API cannot move a tag, so a mismatch
    # has to be said out loud on both paths. A missing tag answers None and stays quiet.
    tag_points_at = None
    try:
        tag_points_at = tag_commit(base, repository, plan["tag"], token)
    except PublishError as error:
        # Only the warning below hangs on this read. Failing a verified publish over it would be
        # worse than the noise, but staying silent is how a misplaced tag goes unnoticed.
        print(f"warning: could not read tag {plan['tag']}: {error}", file=sys.stderr)

    release = release_for_tag(base, repository, plan["tag"], token)
    if release is None:
        created = request(
            "POST",
            f"{base}/api/v1/repos/{repository}/releases",
            token,
            {
                "tag_name": plan["tag"],
                "name": plan["name"],
                "prerelease": plan["prerelease"],
                "draft": False,
                # Without this the server tags whatever its default branch points at when the
                # release is created — the way v2.0.29 ended up tagged at an unrelated commit.
                "target_commitish": commit,
            },
        )
        if not isinstance(created, dict) or "id" not in created:
            raise PublishError(f"the release response did not carry an id: {created!r}")
        print(f"created release {created['id']} for {plan['tag']}")
        release = created
    else:
        print(f"adding to release {release.get('id')} for {plan['tag']}")

    if tag_points_at and tag_points_at != commit:
        print(
            f"warning: tag {plan['tag']} already points at {tag_points_at[:9]}, while these "
            f"artifacts were built from {commit[:9]}; this API cannot move a tag",
            file=sys.stderr,
        )

    # An asset already carrying a name is left alone rather than uploaded again: re-running a
    # platform's publish is a normal thing to do, and a duplicate would give the download page two
    # entries for one file.
    present = {asset.get("name") for asset in release.get("assets", []) if isinstance(asset, dict)}
    uploaded = 0
    for asset in plan["assets"]:
        path = Path(asset)
        if path.name in present:
            print(f"  {path.name} is already published")
            continue
        upload_asset(base, repository, int(release["id"]), path, token)
        uploaded += 1
        print(f"  uploaded {path.name}")
    print(f"\npublished {uploaded} new asset(s) under {plan['tag']}")


def main(argv: list[str]) -> int:
    publish = "--publish" in argv[1:]
    try:
        plan = release_plan()
    except PublishError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    if plan["channel"] not in RELEASABLE_CHANNELS:
        print(f"error: channel {plan['channel']!r} is not a release channel", file=sys.stderr)
        return 1

    total = sum(Path(asset).stat().st_size for asset in plan["assets"])
    print(f"tag        {plan['tag']}  ({'prerelease' if plan['prerelease'] else 'release'})")
    print(f"name       {plan['name']}")
    print(f"repository {plan['repository']} @ {plan['base_url']}")
    print(f"assets     {len(plan['assets'])} files, {total / 1024 / 1024:.1f} MB")
    for asset in plan["assets"]:
        print(f"             {Path(asset).name}")

    try:
        # Read once here: the stamp that gets checked has to be the stamp whose commit is tagged.
        stamp = artifact_stamp()
        problems = publishability_problems(plan, stamp)
        warnings = provenance_warnings(stamp)
    except PublishError as error:
        # The gates answer questions about the working tree, so they can fail on their own
        # (no git, no repository). Refusing is right; a traceback would not say why.
        print(f"error: {error}", file=sys.stderr)
        return 1
    if not publish:
        for warning in warnings:
            print(f"warning: {warning}")
        for problem in problems:
            print(f"warning: {problem}")
        print("\ndry run: pass --publish to create the release")
        return 0

    token = os.environ.get("GITEA_TOKEN", "").strip()
    if not token:
        print(
            "error: GITEA_TOKEN is not set. It is read from the environment on purpose: an "
            "argument would land in the shell history and the process table, and a file would "
            "land in the repository.",
            file=sys.stderr,
        )
        return 1

    if problems:
        for problem in problems:
            print(f"error: {problem}", file=sys.stderr)
        return 1

    for warning in warnings:
        print(f"warning: {warning}")

    try:
        publish_release(plan, stamp, token)
    except PublishError as error:
        # Everything past here talks to the server, where any answer can be an error. Printing the
        # reason beats a traceback that hides which step failed.
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
