#!/usr/bin/env python3
"""Fixture tests for the branding reader.

Run from `oxideterm/`:
    python3 custom-branding/test_branding.py
"""

from __future__ import annotations

import copy
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from branding import BrandingError, load, validate  # noqa: E402

VALID = {
    "productName": "Example",
    "productShortName": "example",
    "appId": "com.example.app",
    "description": "An example product",
    "author": {"name": "Example", "email": "support@example.test"},
    "homepage": "https://example.test",
    "copyright": "Copyright © 2026 Example",
    "executableName": "oxideterm-native",
}


def with_change(**changes):
    config = copy.deepcopy(VALID)
    config.update(changes)
    return config


class BrandingTests(unittest.TestCase):
    def test_the_checked_in_configuration_is_valid(self):
        loaded = load()
        self.assertEqual(loaded["productName"], "RayTerm")
        self.assertEqual(loaded["appId"], "com.rayterm.app")

    def test_a_complete_configuration_is_accepted(self):
        self.assertIs(validate(VALID), VALID)

    def test_every_missing_required_field_is_reported_together(self):
        # A rebrand changes several fields at once, so the reader reports all of them rather than
        # making the author fix one per run.
        for field in ("productName", "appId", "author"):
            config = copy.deepcopy(VALID)
            config.pop(field)
            with self.assertRaises(BrandingError) as caught:
                validate(config)
            self.assertIn(field, str(caught.exception))

    def test_a_blank_required_field_is_a_problem_not_a_value(self):
        with self.assertRaises(BrandingError):
            validate(with_change(productName="   "))

    def test_the_bundle_identifier_must_be_reverse_dns(self):
        for bad in ("RayTerm", "com", "Com.Example.App", "com.example.app!"):
            with self.assertRaises(BrandingError, msg=bad):
                validate(with_change(appId=bad))

    def test_the_executable_name_may_not_change(self):
        # Stored AI presets invoke the binary by this name, so a rebrand that changed it would
        # break presets the user already has.
        with self.assertRaises(BrandingError) as caught:
            validate(with_change(executableName="rayterm"))
        self.assertIn("presets", str(caught.exception))

    def test_a_url_may_not_carry_credentials(self):
        with self.assertRaises(BrandingError) as caught:
            validate(with_change(homepage="https://user:token@example.test"))
        self.assertIn("credentials", str(caught.exception))

    def test_a_url_may_not_carry_a_query_or_fragment(self):
        for bad in (
            "https://example.test/?token=abc",
            "https://example.test/#release",
        ):
            with self.assertRaises(BrandingError, msg=bad):
                validate(with_change(homepage=bad))

    def test_a_url_must_be_https(self):
        with self.assertRaises(BrandingError):
            validate(with_change(homepage="http://example.test"))

    def test_an_empty_optional_url_is_allowed(self):
        # A deployment that has no homepage is a real case; an empty string says so, and the
        # reader must not treat it as a malformed URL.
        validate(with_change(homepage=""))

    def test_a_missing_file_names_the_path(self):
        with self.assertRaises(BrandingError) as caught:
            load(Path("/nonexistent/branding.json"))
        self.assertIn("not found", str(caught.exception))


if __name__ == "__main__":
    unittest.main(verbosity=2)
