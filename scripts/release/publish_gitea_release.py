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

import package_native as packaging  # noqa: E402
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


def request(method: str, url: str, token: str, payload: object | None = None) -> object:
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

    if not publish:
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

    base = plan["base_url"]
    repository = plan["repository"]
    release = request(
        "POST",
        f"{base}/api/v1/repos/{repository}/releases",
        token,
        {
            "tag_name": plan["tag"],
            "name": plan["name"],
            "prerelease": plan["prerelease"],
            "draft": False,
        },
    )
    if not isinstance(release, dict) or "id" not in release:
        raise PublishError(f"the release response did not carry an id: {release!r}")
    print(f"created release {release['id']} for {plan['tag']}")

    for asset in plan["assets"]:
        path = Path(asset)
        upload_asset(base, repository, int(release["id"]), path, token)
        print(f"  uploaded {path.name}")
    print(f"\npublished {plan['tag']} with {len(plan['assets'])} assets")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
