"""Fixed issue1938 Accounts fixture namespaces; source only until the outer plan is admitted.

No command-line entry point, process launch, authority selection or cleanup.
The outer adapter owns admission, all original children and immutable receipts.
"""
import hashlib
import os
from pathlib import Path
import stat
import subprocess

ENGINE_SHA256 = "d9eeec3f2e228e4844695de096f85ecf465381ebd0a2c7a85e964b7103ebe0d9"
SCENES = frozenset(('claude-sparse', 'claude-malformed', 'claude-delayed', 'claude-ticket', 'codex-sparse', 'codex-malformed', 'codex-delayed', 'codex-ticket'))
VIEWPORTS = frozenset(("wide", "narrow"))
XDG_CHILDREN = {
    "XDG_DATA_HOME": "data", "XDG_CONFIG_HOME": "config",
    "XDG_CACHE_HOME": "cache", "XDG_STATE_HOME": "state",
    "XDG_RUNTIME_DIR": "runtime",
}
DIRECTORY_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
# Preserve even a partially constructed object's original descriptors on error.
# The outer adapter records these objects and closes them only after custody.
ALL_NAMESPACES = []


def require(value, message):
    if not value:
        raise RuntimeError(message)


def identity(info):
    return (info.st_dev, info.st_ino, info.st_uid, stat.S_IMODE(info.st_mode))


def private_directory(info):
    require(stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid()
            and stat.S_IMODE(info.st_mode) == 0o700,
            "Fixture directory is not a private original directory")


def closed_original_custody(owner, *, require_scene):
    """Check genuine maintained objects, never a supplied success boolean.

    This is recorded/source-enforced closure, not a kernel close-syscall trace.
    No foreign PID, process-name scan or reopened process handle is involved.
    """
    source = Path(owner.__class__.__init__.__code__.co_filename)
    require(hashlib.sha256(source.read_bytes()).hexdigest() == ENGINE_SHA256,
            "Only the exact maintained process-custody source is admitted")
    require(owner.__class__.__name__ == "Custody" and owner.pid == os.getpid()
            and owner.uid == os.getuid() and owner.closed is True,
            "Namespace transition requires this original wrapper's closed custody")
    if require_scene:
        require(owner.launches > 0 and owner.direct and owner.nodes,
                "An empty completion is not original scene closure")
    owner.scan()
    child_ids = owner.scan.__func__.__globals__["child_ids"]
    require(not child_ids(os.getpid()), "Original wrapper still has children")
    require(all(node.get("settled") is True and "pidfd" not in node
                for node in owner.nodes.values()),
            "Every original family record and pidfd must close before transition")
    waits = []
    for pid, child in owner.direct.items():
        node = owner.nodes[pid]
        require(isinstance(child, subprocess.Popen) and child.pid == pid,
                "Original Popen identity is missing")
        code = child.wait(timeout=0)
        require(code is not None and node.get("exit_code") == code,
                "Original direct wait is incomplete or disagrees with custody")
        waits.append({"pid": pid, "original_exit": code})
    return {"wrapper": owner.wrapper, "launches": owner.launches,
            "all_nodes_nonadditive": len(owner.nodes), "original_waits": waits,
            "pidfds_closed_in_original_records": True,
            "limit": "Source-enforced/recorded closure; no live close-syscall trace"}


class SceneNamespace:
    """One fixed Accounts scene and revision's fresh directory.

    Directories and archives are retained on every failure. No operation here
    deletes, empties, repairs, copies back or retries a namespace.
    """

    def __init__(self, scene, viewport, variant):
        require(scene in SCENES and viewport in VIEWPORTS and variant in ("before", "after"),
                "Only the fixed Accounts scenes, viewports and revision suffixes are admitted")
        # Separate fresh roots; a suffix is a label, never revision authority.
        parent = Path("/tmp")
        self.scene, self.viewport, self.variant = scene, viewport, variant
        self.parent_path, self.purpose = parent, "native"
        self.name = "cagents-1938-native-" + scene + "-" + viewport + "-" + variant
        self.path = parent / self.name
        self.archive_name = None
        self.parent_fd = None
        self.parent_identity = None
        self.root_fd = None
        self.root_identity = None
        self.child_fds = {}
        self.child_identities = {}
        self.archived = False
        self.archive_attempted = False
        self.closure = None
        self.after = None
        self.descriptors_closed = False
        self.close_attempted = False
        self.initialized = False
        self.final_closure = None
        self.close_in_progress = None
        self.last_completed_operation = "internal-record-created"
        self.last_authority_check = None
        ALL_NAMESPACES.append(self)  # Also retain partially created originals.
        self.parent_fd = os.open(parent, DIRECTORY_FLAGS)
        self.parent_identity = identity(os.fstat(self.parent_fd))
        self.last_completed_operation = "original-parent-pinned"
        # O_EXCL directory creation is the authority: never reuse an old entry.
        os.mkdir(self.name, 0o700, dir_fd=self.parent_fd)
        self.last_completed_operation = "new-root-created"
        self.root_fd = os.open(self.name, DIRECTORY_FLAGS, dir_fd=self.parent_fd)
        self.last_completed_operation = "original-root-opened"
        private_directory(os.fstat(self.root_fd))
        self.root_identity = identity(os.fstat(self.root_fd))
        for child in XDG_CHILDREN.values():
            os.mkdir(child, 0o700, dir_fd=self.root_fd)
            descriptor = os.open(child, DIRECTORY_FLAGS, dir_fd=self.root_fd)
            # Register the actual descriptor before ANY fallible observation.
            self.child_fds[child] = (descriptor, None)
            self.child_identities[child] = None
            self.last_completed_operation = "original-child-opened:" + child
            metadata = os.fstat(descriptor)
            expected = identity(metadata)
            self.child_fds[child] = (descriptor, expected)
            self.child_identities[child] = expected
            private_directory(metadata)
            require(os.listdir(descriptor) == [], "New XDG child is not empty")
        os.fsync(self.root_fd)
        os.fsync(self.parent_fd)
        self.check()
        self.initialized = True
        self.last_completed_operation = "new-namespace-fully-initialized"

    def check(self, *, archived=False):
        try:
            self._check_live(archived=archived)
        except BaseException as error:
            self.last_authority_check = {"state": "failed-current-authority-unavailable",
                                         "error": {"type": type(error).__name__, "message": str(error)}}
            raise
        self.last_authority_check = {"state": "verified-at-explicit-check"}

    def _check_live(self, *, archived):
        require(not self.descriptors_closed and self.root_fd is not None,
                "Original namespace descriptors are unavailable")
        require(archived == self.archived, "Wrong namespace lifetime")
        require(identity(os.fstat(self.parent_fd)) == self.parent_identity
                == identity(os.stat(self.parent_path, follow_symlinks=False)),
                "Original namespace parent changed")
        name = self.archive_name if archived else self.name
        held = os.fstat(self.root_fd)
        named = os.stat(name, dir_fd=self.parent_fd, follow_symlinks=False)
        private_directory(held)
        private_directory(named)
        require(identity(held) == self.root_identity == identity(named),
                "Named namespace no longer identifies its original directory")
        for child, (descriptor, expected) in self.child_fds.items():
            held = os.fstat(descriptor)
            named = os.stat(child, dir_fd=self.root_fd, follow_symlinks=False)
            private_directory(held)
            private_directory(named)
            require(identity(held) == expected == identity(named),
                    "Original XDG child changed")

    def environment_paths(self):
        self.check()
        return {key: str(self.path / name) for key, name in XDG_CHILDREN.items()}

    def close_original_descriptors(self, owner):
        require(not self.close_attempted, "Descriptor closure is never replayed")
        proof = closed_original_custody(owner, require_scene=False)
        if self.initialized:
            self.check(archived=self.archived)
        require(self.after is None or self.after.descriptors_closed,
                "Keep Before's original inode pinned until After has closed")
        self.close_attempted = True
        for name, (descriptor, _expected) in tuple(self.child_fds.items()):
            self.close_in_progress = {"kind": "child", "name": name, "descriptor": descriptor}
            os.close(descriptor)
            del self.child_fds[name]
            self.close_in_progress = None
            self.last_completed_operation = "original-child-closed:" + name
        if self.root_fd is not None:
            self.close_in_progress = {"kind": "root", "descriptor": self.root_fd}
            os.close(self.root_fd)
            self.root_fd = None
            self.close_in_progress = None
            self.last_completed_operation = "original-root-descriptor-closed"
        if self.parent_fd is not None:
            self.close_in_progress = {"kind": "parent", "descriptor": self.parent_fd}
            os.close(self.parent_fd)
            self.parent_fd = None
            self.close_in_progress = None
            self.last_completed_operation = "original-parent-descriptor-closed"
        self.descriptors_closed = True
        self.final_closure = proof
        return proof

    def receipt(self):
        # This method is diagnostic data only. In particular, never repeat a
        # failing live check or touch a possibly closed/reused descriptor here.
        return {"scene": self.scene, "viewport": self.viewport, "variant": self.variant,
                "purpose": self.purpose,
                "visible_path": str(self.path),
                "retained_path": str(self.parent_path / (self.archive_name if self.archived else self.name)),
                "root_identity": self.root_identity,
                "children": dict(self.child_identities),
                "initialized": self.initialized,
                "original_root_was_pinned": self.root_identity is not None,
                "descriptors_closed": self.descriptors_closed,
                "close_attempted": self.close_attempted,
                "remaining_or_unknown_original_descriptors": len(self.child_fds)
                    + int(self.root_fd is not None) + int(self.parent_fd is not None),
                "close_in_progress_or_unknown": self.close_in_progress,
                "last_completed_operation": self.last_completed_operation,
                "last_explicit_authority_check": self.last_authority_check,
                "current_named_authority": "not-checked-or-asserted-by-this-diagnostic-receipt",
                "archive_attempted": self.archive_attempted,
                "archived": self.archived, "closure": self.closure,
                "final_closure": self.final_closure,
                "files_deleted_or_reused": False}
