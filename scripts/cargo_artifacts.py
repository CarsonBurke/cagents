"""Read Cargo JSON and select reported binary executables without guessing paths.

Callers own the command, its output sinks and its lifetime. Validation is deferred
until executable() so a failed Cargo command can keep its original exit status.
This module travels beside the build-gate helper and uses only the standard library.
"""

import json
import os
from pathlib import Path


class CargoArtifactError(RuntimeError):
    pass


class CargoArtifacts:
    def __init__(self):
        self._products = {}
        self._errors = {}
        self._stream_error = None

    def read(self, line, *, diagnostics=None):
        """Read one line, optionally forwarding rendered diagnostics and noise.

        Return the decoded message for callers that also inspect build-script
        records. Plain output from wrappers is forwarded, never parsed as a path.
        """
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            if diagnostics is not None:
                diagnostics.write(line)
            return None
        if not isinstance(message, dict):
            self._stream_error = self._stream_error or "Cargo emitted a non-object JSON message"
            return None
        if diagnostics is not None and message.get("reason") == "compiler-message":
            diagnostic = message.get("message")
            rendered = diagnostic.get("rendered") if isinstance(diagnostic, dict) else None
            if rendered is not None and not isinstance(rendered, str):
                self._stream_error = self._stream_error or "Cargo emitted a malformed compiler diagnostic"
            elif rendered:
                diagnostics.write(rendered)
        if message.get("reason") != "compiler-artifact":
            return message
        target = message.get("target")
        if not isinstance(target, dict) or not isinstance(target.get("name"), str):
            self._stream_error = self._stream_error or "Cargo emitted a malformed artifact target"
            return message
        name = target["name"]
        kinds = target.get("kind")
        if not isinstance(kinds, list) or not all(isinstance(kind, str) for kind in kinds):
            self._errors.setdefault(name, f"Cargo reported an invalid target kind for {name}")
            return message
        if "bin" not in kinds or message.get("executable") is None:
            return message
        product = message["executable"]
        if not isinstance(product, str) or "\0" in product or not os.path.isabs(product):
            self._errors.setdefault(name, f"Cargo reported an invalid executable path for {name}")
        else:
            self._products.setdefault(name, set()).add(product)
        return message

    def executable(self, name):
        """Require one distinct, absolute, runnable binary artifact for NAME."""
        error = self._stream_error or self._errors.get(name)
        if error:
            raise CargoArtifactError(error)
        products = self._products.get(name, set())
        if len(products) != 1:
            raise CargoArtifactError(
                f"Cargo did not report exactly one executable for {name} (found {len(products)})"
            )
        product = Path(next(iter(products)))
        if not product.is_file() or not os.access(product, os.X_OK):
            raise CargoArtifactError(f"Cargo executable for {name} is missing or not runnable: {product}")
        return product
