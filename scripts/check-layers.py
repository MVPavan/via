#!/usr/bin/env python3
"""Check that VIA crate dependencies match the approved downward graph."""

import json
import subprocess
import sys
import tomllib
from pathlib import Path


ALLOWED = {
    "via-cli": {"via-core"},
    "via-core": {"via-adapters", "via-store"},
    "via-adapters": {"via-routes"},
    "via-routes": {"via-wire"},
    "via-wire": {"via-host", "via-store"},
    "via-host": {"via-store"},
    "via-store": set(),
}


SECTIONS = {
    None: "dependencies",
    "dev": "dev-dependencies",
    "build": "build-dependencies",
}


def manifest_dependency(manifest, dependency):
    """Find a Cargo metadata dependency in its manifest table."""
    target = dependency["target"]
    table = manifest if target is None else manifest.get("target", {}).get(target, {})
    section = table.get(SECTIONS[dependency["kind"]], {})
    alias = dependency["rename"] or dependency["name"]
    return alias, section.get(alias)


def main():
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        check=True,
        capture_output=True,
        text=True,
    )
    metadata = json.loads(result.stdout)
    members = set(metadata["workspace_members"])
    packages = {p["name"]: p for p in metadata["packages"] if p["id"] in members}
    if set(packages) != set(ALLOWED):
        print("Unexpected workspace crates:", sorted(set(packages) ^ set(ALLOWED)), file=sys.stderr)
        return 1

    root_manifest = Path(metadata["workspace_root"]) / "Cargo.toml"
    workspace = tomllib.loads(root_manifest.read_text())
    workspace_dependencies = workspace["workspace"].get("dependencies", {})
    failed = False
    for name, spec in workspace_dependencies.items():
        if isinstance(spec, dict) and "git" in spec:
            print(f"workspace dependency {name}: git source is forbidden", file=sys.stderr)
            failed = True
    for registry, replacements in workspace.get("patch", {}).items():
        for name, spec in replacements.items():
            if isinstance(spec, dict) and "git" in spec:
                print(f"patch {registry}.{name}: git source is forbidden", file=sys.stderr)
                failed = True
    for name, spec in workspace.get("replace", {}).items():
        if isinstance(spec, dict) and "git" in spec:
            print(f"replacement {name}: git source is forbidden", file=sys.stderr)
            failed = True

    for source, package in sorted(packages.items()):
        manifest = tomllib.loads(Path(package["manifest_path"]).read_text())
        actual = set()
        for dependency in package["dependencies"]:
            name = dependency["name"]
            alias, spec = manifest_dependency(manifest, dependency)
            if spec is None:
                print(f"{source}: cannot find manifest entry for {alias}", file=sys.stderr)
                failed = True
                continue
            if dependency["source"] and dependency["source"].startswith("git+"):
                print(f"{source}: git source for {alias} is forbidden", file=sys.stderr)
                failed = True
            if isinstance(spec, dict) and "git" in spec:
                print(f"{source}: git source for {alias} is forbidden", file=sys.stderr)
                failed = True

            if name.startswith("via-"):
                actual.add(name)
                expected_path = Path(packages[name]["manifest_path"]).parent if name in packages else None
                if (dependency["source"] is not None or dependency["path"] is None
                        or expected_path is None
                        or Path(dependency["path"]).resolve() != expected_path.resolve()):
                    print(f"{source}: {name} must use its workspace path", file=sys.stderr)
                    failed = True
            elif not (isinstance(spec, dict) and spec.get("workspace") is True
                      and alias in workspace_dependencies):
                print(f"{source}: external dependency {alias} must inherit from the workspace", file=sys.stderr)
                failed = True

        for target in sorted(actual):
            print(f"{source} -> {target}")
        unexpected = actual - ALLOWED[source]
        if unexpected:
            print(
                f"{source}: unexpected internal edges to {sorted(unexpected)}",
                file=sys.stderr,
            )
            failed = True
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
