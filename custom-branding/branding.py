#!/usr/bin/env python3
"""Read the product's branding.

This is the single place the product's identity is stated. It exists so that a rebrand, or a
second deployment of the same client, is a change to data rather than a hunt through Rust
sources, locale files and release scripts — and so that an upstream merge never has to
reconcile a scattered rename.

Nothing consumes it yet beyond this module and its tests: the callers still carry the same
values inline. Moving them over is deliberate and incremental, because a single sweep across
every branded string is a change nobody can review.
"""

from __future__ import annotations

import json
import re
from pathlib import Path

CONFIG = Path(__file__).resolve().parent / "config" / "branding.json"

REQUIRED = (
    "productName",
    "productShortName",
    "appId",
    "description",
    "executableName",
)

# Reverse-DNS with at least two segments, which is what a macOS bundle identifier has to be.
APP_ID = re.compile(r"^[a-z][a-z0-9]*(\.[a-z0-9][a-z0-9-]*)+$")


class BrandingError(ValueError):
    """The configuration is not usable as written."""


def _check_distribution_url(field: str, value: str) -> None:
    """Same rules as a public URL, except that plain http is allowed.

    A distribution server inside the deployment's own network is a real case, and requiring https
    there would only push the value out of this file and back into a script. The rules that exist
    for a shipped value still apply: no credentials, no query, no fragment.
    """
    if not value:
        return
    remainder = value.split("://", 1)[-1]
    if "@" in remainder.split("/")[0]:
        raise BrandingError(f"{field} must not carry credentials: {value!r}")
    if "?" in value or "#" in value:
        raise BrandingError(f"{field} must not carry a query or fragment: {value!r}")


def _check_url(field: str, value: str) -> None:
    """Rejects a URL that must not be shipped inside an application.

    Credentials in a URL would be published with every build. A query string or fragment is
    rejected for the same reason a fragment is useless here: both are the shape of a
    pre-signed or tracked link, and neither belongs in a value embedded in a binary.
    """
    if not value:
        return
    if not value.startswith("https://"):
        raise BrandingError(f"{field} must be https, got {value!r}")
    remainder = value[len("https://") :]
    if "@" in remainder.split("/")[0]:
        raise BrandingError(f"{field} must not carry credentials: {value!r}")
    if "?" in value or "#" in value:
        raise BrandingError(f"{field} must not carry a query or fragment: {value!r}")


def validate(config: object, *, source: str = "branding.json") -> dict:
    """Returns the configuration, or raises with every problem it found.

    All problems are reported at once rather than at the first: a rebrand touches several fields
    together, and fixing them one error per run is needless.
    """
    if not isinstance(config, dict):
        raise BrandingError(f"{source} must contain a JSON object")

    problems: list[str] = []
    for field in REQUIRED:
        value = config.get(field)
        if not isinstance(value, str) or not value.strip():
            problems.append(f"{field} is required and must be a non-empty string")

    app_id = config.get("appId")
    if isinstance(app_id, str) and app_id and not APP_ID.match(app_id):
        problems.append(f"appId must be reverse-DNS (for example com.example.app), got {app_id!r}")

    executable = config.get("executableName")
    if isinstance(executable, str) and executable:
        # The shipped binary's name is also the ACP adapter command that stored AI presets
        # invoke, so renaming it silently breaks presets a user already has. It is allowed to
        # differ from the product name, and it must not be changed by a rebrand.
        if executable != "oxideterm-native":
            problems.append(
                "executableName must stay 'oxideterm-native': stored AI presets invoke it by "
                f"that name, so changing it breaks them (got {executable!r})"
            )

    author = config.get("author")
    if not isinstance(author, dict) or not str(author.get("name", "")).strip():
        problems.append("author.name is required")

    for field in ("homepage", "copyright"):
        value = config.get(field, "")
        if not isinstance(value, str):
            problems.append(f"{field} must be a string")
    for field in ("homepage",):
        value = config.get(field)
        if isinstance(value, str):
            try:
                _check_url(field, value)
            except BrandingError as error:
                problems.append(str(error))

    distribution = config.get("distribution")
    if not isinstance(distribution, dict):
        problems.append("distribution is required and must be an object")
    else:
        for field in ("giteaUrl", "repository"):
            value = distribution.get(field)
            if not isinstance(value, str) or not value.strip():
                problems.append(f"distribution.{field} is required")
        for field in ("giteaUrl",):
            value = distribution.get(field)
            if isinstance(value, str):
                try:
                    _check_distribution_url(f"distribution.{field}", value)
                except BrandingError as error:
                    problems.append(str(error))

    if problems:
        raise BrandingError(f"{source} is not usable:\n  - " + "\n  - ".join(problems))
    return config


def load(path: Path | None = None) -> dict:
    """Reads and validates the branding."""
    config_path = path or CONFIG
    try:
        raw = json.loads(config_path.read_text(encoding="utf-8"))
    except FileNotFoundError as error:
        raise BrandingError(f"branding config not found at {config_path}") from error
    except json.JSONDecodeError as error:
        raise BrandingError(f"{config_path} is not valid JSON: {error}") from error
    return validate(raw, source=config_path.name)


if __name__ == "__main__":
    import sys

    try:
        loaded = load()
    except BrandingError as error:
        print(error, file=sys.stderr)
        raise SystemExit(1) from error
    print(json.dumps(loaded, ensure_ascii=False, indent=2))
