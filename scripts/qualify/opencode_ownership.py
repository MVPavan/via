"""Persistent run-root ownership for OpenCode qualification (packet §13)."""
from dataclasses import dataclass, replace
import os
from pathlib import Path
import stat

from opencode_safety import Blocked

KINDS = {'via-owned', 'vendor-private', 'runner-evidence', 'helper'}


@dataclass(frozen=True)
class Root:
    """Fixed ownership; daemon-start status belongs to its state (§13 L11)."""
    path: Path
    kind: str
    directory: bool = True
    recursive: bool = True
    state: bool = False
    started: bool = False
    runtime: bool = False


class OwnershipRegistry:
    """Retain every root; classify by longest prefix without resolving links (§13)."""
    def __init__(self, evidence):
        self.roots = {}
        self.evidence = Path(os.path.abspath(evidence))
        # A discovery boundary is not a blanket claim on unexpected evidence.
        self.register(self.evidence, 'runner-evidence', recursive=False)

    def register(self, path, kind, *, directory=True, recursive=True, state=False, runtime=False):
        """Claim at creation/handoff; reject conflicting ownership forever (§13)."""
        path = Path(os.path.abspath(path))
        if kind not in KINDS or state and kind != 'via-owned' or runtime and kind != 'via-owned':
            raise Blocked('invalid ownership registration')
        candidate = Root(path, kind, directory, recursive and directory, state, runtime=runtime)
        previous = self.roots.get(path)
        if previous is not None and replace(previous, started=False) != candidate:
            raise Blocked('ownership root cannot be reclassified')
        if previous is None:
            self.roots[path] = candidate
        return path

    def register_state(self, path):
        """Own a state and privatize its entire vendor tree, including probes (§13)."""
        path = self.register(path, 'via-owned', state=True)
        self.register(path / 'vendor', 'vendor-private')
        return path

    def daemon_started(self, path):
        """Require a Store only for this verified started state (§13 L11)."""
        path = Path(os.path.abspath(path))
        root = self.roots.get(path)
        if root is None or not root.state:
            raise Blocked('daemon state was not registered')
        self.roots[path] = replace(root, started=True)

    def ordered(self):
        """Stable traversal/backup order independent of Python hash seeds (§13)."""
        return sorted(self.roots.values(), key=lambda root: str(root.path))

    def classify(self, path):
        """The longest registered prefix owns a path, including old generations (§13)."""
        path = Path(os.path.abspath(path))
        matches = [root for root in self.roots.values()
                   if path == root.path or root.recursive and path.is_relative_to(root.path)]
        return max(matches, key=lambda root: len(root.path.parts), default=None)

    def walks(self):
        """Walk each registered tree once, including roots outside evidence (§13)."""
        selected = []
        for root in self.ordered():
            if not root.directory:
                continue
            if not any(root.path.is_relative_to(parent) for parent in selected):
                selected.append(root.path)
        return selected

    def allowed_nonregular(self, path, mode):
        """Only the checked rg link and named runtime sockets may be skipped (§13)."""
        for root in self.ordered():
            if root.kind == 'helper' and root.path.name == 'helpers' \
                    and path == root.path / 'rg' and stat.S_ISLNK(mode):
                return True
            if root.runtime and stat.S_ISSOCK(mode) and (path == root.path / 'via.sock'
                    or path.parent == root.path / 'anchors' and path.suffix == '.sock'):
                return True
        return False
