#!/usr/bin/env python3
"""Keep every vendored Cargo patch anchored to the checkout that built it."""

import importlib.util
from pathlib import Path
import sys
import unittest


sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "checkout_cache_qualifier", ROOT / "scripts/qualify-checkout-cache.py"
)
QUALIFIER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(QUALIFIER)


class VendorHooks(unittest.TestCase):
    def test_patched_vendors_have_checkout_identity_hooks(self):
        QUALIFIER.guard_vendor_hooks()


if __name__ == "__main__":
    unittest.main()
