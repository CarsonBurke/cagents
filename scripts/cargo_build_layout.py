"""Read-only inventories of Cargo's private build layout.

CargoDirectories consumes decoded `cargo metadata --format-version 1` output;
it never invokes Cargo. BuildProfile recognizes the legacy .fingerprint/deps
layout and the per-unit build/<package>/<16-hex-hash> layout. Paths are reported
without resolving symlinks or descending into arbitrary build-script OUT_DIRs.

An unknown or unsafe profile has known_layout=False and empty inventories:
callers must let Cargo decide instead of using a freshness or seeding shortcut.
These are snapshots, not leases. Retention, locking and mutation belong to the
caller, including revalidation before deleting anything.
"""

from dataclasses import dataclass
from pathlib import Path
import re
import stat


UNIT_HASH = re.compile(r"[0-9a-f]{16}")
LEGACY_UNIT = re.compile(r"(.+)-([0-9a-f]+)")


def _directory(path):
    """Require real directory components, allowing an absent final directory."""
    for component in (*reversed(path.parents), path):
        try:
            mode = component.lstat().st_mode
        except FileNotFoundError:
            return False
        if not stat.S_ISDIR(mode):
            raise ValueError(f"not a real directory: {component}")
    return True


def _entries(path):
    if not _directory(path):
        return ()
    entries = tuple(sorted(path.iterdir()))
    for entry in entries:
        if stat.S_ISLNK(entry.lstat().st_mode):
            raise ValueError(f"symlink in build inventory: {entry}")
    return entries


def _files(path, suffix):
    return tuple(entry for entry in _entries(path)
                 if entry.suffix == suffix and stat.S_ISREG(entry.lstat().st_mode))


@dataclass(frozen=True)
class ArtifactUnit:
    package: str
    unit_hash: str
    fingerprint: Path
    # Legacy units consist of a fingerprint followed by matching deps/build
    # entries. New units consist of one whole directory. This is membership,
    # not permission to evict any of these paths.
    parts: tuple[Path, ...]
    dependency_lists: tuple[Path, ...] = ()
    build_script_outputs: tuple[Path, ...] = ()


@dataclass(frozen=True)
class BuildProfile:
    target_directory: Path
    build_directory: Path
    layout: str | None = None
    units: tuple[ArtifactUnit, ...] = ()
    dependency_lists: tuple[Path, ...] = ()
    build_script_outputs: tuple[Path, ...] = ()

    @property
    def known_layout(self):
        return self.layout is not None

    @classmethod
    def read(cls, target_directory, build_directory):
        """Inventory one profile; any uncertain private structure disables it.

        Dependency lists include public product .d files, legacy deps/*.d and
        per-unit out/*.d. Build-script output means only Cargo's output or
        run/stdout file, never similarly named files inside an OUT_DIR.
        """
        target, build = Path(target_directory), Path(build_directory)
        unknown = cls(target, build)
        try:
            if not _directory(build):
                return unknown
            # Do not mistake a supported subset for a complete private layout
            # when Cargo introduces another directory alongside it.
            for entry in _entries(build):
                if (stat.S_ISDIR(entry.lstat().st_mode)
                        and entry.name not in {".fingerprint", "deps", "build", "incremental", "examples"}):
                    return unknown
            fingerprints = build / ".fingerprint"
            legacy = _directory(fingerprints)
            units = []
            legacy_units = {}
            for fingerprint in _entries(fingerprints):
                match = LEGACY_UNIT.fullmatch(fingerprint.name)
                if not match or not _directory(fingerprint):
                    return unknown
                package, unit_hash = match.groups()
                if unit_hash in legacy_units:
                    return unknown
                legacy_units[unit_hash] = (package, fingerprint)

            outputs = {unit_hash: [] for unit_hash in legacy_units}
            dependencies = list(_files(target, ".d"))
            for output in _entries(build / "deps"):
                match = LEGACY_UNIT.fullmatch(output.name.split(".", 1)[0])
                if not match or match[2] not in legacy_units:
                    return unknown
                if not stat.S_ISREG(output.lstat().st_mode):
                    return unknown
                outputs[match[2]].append(output)

            modern = False
            for package in _entries(build / "build"):
                if not _directory(package):
                    return unknown
                match = LEGACY_UNIT.fullmatch(package.name)
                if legacy and match and match[2] in legacy_units:
                    # Do not inspect the contents of a legacy OUT_DIR.
                    outputs[match[2]].append(package)
                    continue
                hashes = _entries(package)
                if not hashes:
                    return unknown
                for unit in hashes:
                    if not UNIT_HASH.fullmatch(unit.name) or not _directory(unit):
                        return unknown
                    _entries(unit)
                    fingerprint = unit / "fingerprint"
                    if not _directory(fingerprint):
                        return unknown
                    # Check the structural directories without walking files
                    # generated beneath out/. Only immediate .d lists count.
                    _entries(fingerprint)
                    unit_dependencies = _files(unit / "out", ".d")
                    stdout = unit / "run/stdout"
                    run_entries = _entries(unit / "run")
                    script_outputs = (stdout,) if stdout in run_entries else ()
                    if script_outputs and not stat.S_ISREG(stdout.lstat().st_mode):
                        return unknown
                    units.append(ArtifactUnit(package.name, unit.name, fingerprint,
                                              (unit,), unit_dependencies, script_outputs))
                    modern = True

            for unit_hash, (package, fingerprint) in legacy_units.items():
                _entries(fingerprint)
                parts = (fingerprint, *outputs[unit_hash])
                unit_dependencies = tuple(path for path in outputs[unit_hash]
                                          if path.parent == build / "deps" and path.suffix == ".d")
                script_outputs = []
                for path in outputs[unit_hash]:
                    if path.parent == build / "build":
                        # Only the immediate Cargo record, not out/output.
                        entries = _entries(path)
                        output = path / "output"
                        if output in entries:
                            if not stat.S_ISREG(output.lstat().st_mode):
                                return unknown
                            script_outputs.append(output)
                units.append(ArtifactUnit(package, unit_hash, fingerprint, parts,
                                          unit_dependencies, tuple(script_outputs)))

            if not legacy and not modern:
                return unknown
            units.sort(key=lambda unit: (unit.package, unit.unit_hash, unit.fingerprint))
            dependencies.extend(path for unit in units for path in unit.dependency_lists)
            script_outputs = tuple(sorted(path for unit in units for path in unit.build_script_outputs))
            layout = "mixed" if legacy and modern else "legacy" if legacy else "unit"
            return cls(target, build, layout, tuple(units), tuple(sorted(set(dependencies))), script_outputs)
        except (OSError, ValueError):
            return unknown


@dataclass(frozen=True)
class CargoDirectories:
    target_directory: Path
    build_directory: Path

    @classmethod
    def from_metadata(cls, metadata):
        """Return None for unusable metadata; older Cargo omits build_directory."""
        if not isinstance(metadata, dict):
            return None
        target = metadata.get("target_directory")
        build = metadata.get("build_directory")
        if build is None or build == "":
            build = target
        if not all(isinstance(path, str) and "\0" not in path and Path(path).is_absolute()
                   for path in (target, build)):
            return None
        return cls(Path(target), Path(build))

    def profile(self, relative):
        """Read <profile> or <target-triple>/<profile>, never arbitrary subtrees."""
        relative = Path(relative)
        if relative.is_absolute() or len(relative.parts) not in (1, 2) or ".." in relative.parts:
            raise ValueError("expected a profile or target-triple/profile")
        return BuildProfile.read(self.target_directory / relative, self.build_directory / relative)

    def profiles(self):
        """Discover profiles at Cargo's two depths, without entering their outputs.

        Unknown candidates remain explicit unknown snapshots. A caller must
        not treat an empty inventory as proof that a shortcut is safe.
        """
        profiles = []
        try:
            for first in _entries(self.build_directory):
                if first.name.startswith(".") or not stat.S_ISDIR(first.lstat().st_mode):
                    continue
                relative = first.relative_to(self.build_directory)
                profile = self.profile(relative)
                # A known profile or private markers identifies the boundary,
                # even when those markers are in an unsupported layout.
                names = {entry.name for entry in _entries(first)}
                if profile.known_layout or names & {".fingerprint", "deps", "build", "incremental"}:
                    profiles.append(profile)
                    continue
                children = [entry for entry in _entries(first)
                            if not entry.name.startswith(".") and stat.S_ISDIR(entry.lstat().st_mode)]
                if not children:
                    profiles.append(profile)
                for second in children:
                    profiles.append(self.profile(second.relative_to(self.build_directory)))
        except (OSError, ValueError):
            # Do not return a partial tree as a complete inventory.
            return (BuildProfile(self.target_directory, self.build_directory),)
        return tuple(profiles)
