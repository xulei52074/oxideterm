#!/usr/bin/env python3
"""Write the product name from the branding config into the locale catalogs.

Only the keys whose whole value *is* the product name are written. Prose that happens to mention
the name is left alone: substituting a name into a sentence needs the sentence rewritten, and a
mechanical replacement breaks grammar in languages that order words differently — which is how the
catalogs ended up shipping "RayTerm AI AI" from an earlier rename.

Run from `oxideterm/`:
    python3 custom-branding/sync_product_name.py           # write
    python3 custom-branding/sync_product_name.py --check   # fail if any catalog disagrees

The generated values are committed, the same way the icons are: the config is the source, the
catalogs are outputs, and `--check` is what catches the two drifting apart.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from branding import load  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
LOCALES = ROOT / "crates" / "oxideterm-i18n" / "locales"

# (catalog file, dotted key path). Each of these holds the product name and nothing else.
PRODUCT_NAME_KEYS = (
    ("common", ("app", "name")),
    ("common", ("layout", "empty", "title")),
    ("menu", ("menu", "app")),
)


def catalogs() -> list[Path]:
    return sorted(LOCALES.glob("*/*.json"))


def _read(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def _current(document: dict, dotted: tuple[str, ...]) -> object:
    node: object = document
    for part in dotted:
        if not isinstance(node, dict) or part not in node:
            return None
        node = node[part]
    return node


def _assign(document: dict, dotted: tuple[str, ...], value: str) -> None:
    node = document
    for part in dotted[:-1]:
        node = node[part]
    node[dotted[-1]] = value


def main(argv: list[str]) -> int:
    check_only = "--check" in argv[1:]
    name = load()["productName"]

    changed: list[str] = []
    drifted: list[str] = []

    for path in catalogs():
        document = _read(path)
        touched = False
        for catalog, dotted in PRODUCT_NAME_KEYS:
            if path.stem != catalog:
                continue
            current = _current(document, dotted)
            if current is None:
                # A missing key is a problem the i18n audit owns, not this script: reporting it
                # here too would give two places the same failure and one of them would rot.
                continue
            if current == name:
                continue
            if check_only:
                drifted.append(f"{path.parent.name}/{path.name}: {'.'.join(dotted)} = {current!r}")
            else:
                _assign(document, dotted, name)
                touched = True
        if touched:
            path.write_text(
                json.dumps(document, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
            )
            changed.append(f"{path.parent.name}/{path.name}")

    if check_only:
        if drifted:
            print(f"product name is {name!r}, but these disagree:", file=sys.stderr)
            for line in drifted:
                print(f"  - {line}", file=sys.stderr)
            print(
                "\nRun: python3 custom-branding/sync_product_name.py",
                file=sys.stderr,
            )
            return 1
        print(f"every catalog agrees on {name!r}")
        return 0

    if changed:
        print(f"wrote {name!r} into {len(changed)} catalogs:")
        for line in changed:
            print(f"  {line}")
    else:
        print(f"every catalog already says {name!r}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
