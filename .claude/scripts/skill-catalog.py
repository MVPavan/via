#!/usr/bin/env python3
"""Skill-catalog generator + slash-pointer linter for the skill-router skill.

The router (.claude/skills/skill-router/SKILL.md) is hand-owned above the
marker line and generated below it. Hand-maintained catalogs rot — a previous
router shipped dead pointers undetected until an audit — so this script
generates the mechanical part and lints the rest.

  --check (default)
      Exit 1 if the generated section is stale; if any /name reference in any
      shipped text file under a skill folder (*.md, *.sh, *.py, *.yaml —
      references, scripts, assets included) resolves to no installed skill and
      no ALLOWED_SLASHES entry; if any `.claude/...` path token in those same
      files names nothing on disk under the repo root (placeholder/glob tokens
      are skipped); or if any skill's `agents/openai.yaml` is missing or its
      `policy.allow_implicit_invocation` disagrees with the SKILL.md
      `disable-model-invocation` flag (the Codex twin of that flag).
  --write
      Regenerate everything below the marker line and every skill's
      `agents/openai.yaml` policy block (an existing `interface:` block is
      kept verbatim). Refuses if the marker is missing. Deterministic (sorted)
      and idempotent.

Slash commands were folded into slash-only skills (2026-08-19): there is no
.claude/commands directory; a user-run workflow is a skill with
`disable-model-invocation: true`.

Warnings (reported, never exit-code failures): a quoted trigger phrase claimed
by two or more model-invocable skill descriptions; an ALLOWED_SLASHES entry no
longer referenced anywhere; a commented-out disable-model-invocation key the
loader ignores.

Pure standard library; mirrors harness_lifecycle/gap.py conventions.
"""

from __future__ import annotations

import argparse
import difflib
import re
from dataclasses import dataclass
from pathlib import Path

MARKER = "<!-- generated:skill-catalog -->"
ROUTER_REL = Path("skill-router/SKILL.md")  # relative to the skills dir
WRITE_CMD = "python3 .claude/scripts/skill-catalog.py --write"

# Slash tokens that legitimately resolve outside .claude/skills. One-word
# reason each; --check warns when an entry stops
# being referenced so the list stays minimal.
ALLOWED_SLASHES: dict[str, str] = {
    "/subtask": "builtin",  # Claude Code full-context fork; agent-matrix context modes
}

# `.claude/...` path tokens that legitimately name nothing on disk (an
# illustrative example, a path that exists only in an adopted repo). One-word
# reason each; --check warns when an entry stops being referenced.
ALLOWED_PATHS: dict[str, str] = {
    # Regression fixture proves foreign legacy overlays are preserved by migration.
    ".claude/project": "foreign-repo",
    # migrate-claude-to-codex converts OTHER repos' command dirs; this repo has none.
    ".claude/commands": "foreign-repo",
    ".claude/commands/": "foreign-repo",
    # Migration handles OTHER repos' rule dirs; local guidance is in .repo-context/.
    ".claude/rules": "foreign-repo",
    ".claude/rules/": "foreign-repo",
}

# Text files under a skill folder whose slash and path references are linted.
# Everything else (images, fixtures, binaries) is skipped by suffix.
SHIPPED_TEXT_SUFFIXES = frozenset({".md", ".sh", ".py", ".yaml", ".yml"})

# A slash reference: /name preceded by line start, whitespace, an opening
# paren/bracket/asterisk/quote, or an OPENING backtick (a backtick preceded by
# one of those, or at line start — a closing backtick as in `eval`/`exec` does
# not open a ref). Not a path segment (next char `/`), not a filename
# (`.ext`), not a compound word (next char `-`). Colon admits plugin /ns:cmd.
SLASH_REF = re.compile(
    r"(?:^|(?<=[\s(*\[\"])|(?<=^`)|(?<=[\s(*\[\"]`))"
    r"(/[a-z](?:[a-z0-9:-]*[a-z0-9])?)"
    r"(?!\.\w|[\w/-])"
)

# A repo-relative harness path token, backticked or bare: `.claude/...` not
# preceded by `/`, `~`, or a word char — so `~/.claude/...` (home dir) and
# `<repo>/.claude/...` (another checkout) are out of scope. Captures
# placeholder characters too, so a glob-ish token is recognised whole and
# skipped rather than half-matched; `:NN` line suffixes and `#fragments` end
# the token naturally. Trailing sentence periods are stripped afterwards.
CLAUDE_PATH = re.compile(r"(?<![\w/~])((?:\.claude|\.repo-context)/[\w./\-<>*{}…]*)")
PLACEHOLDER_MARKS = ("<", ">", "*", "{", "}", "…", "...")

# A backticked bare skill/command name, as the hand-owned router table uses.
BARE_NAME = re.compile(r"`([a-z][a-z0-9-]*)`")

# A quoted trigger phrase inside a description: 'like this' or "like this",
# opened after whitespace/punctuation so apostrophes ("project's") don't pair.
QUOTED_PHRASE = re.compile(
    r"(?:(?<=[\s(/:—-])|^)(?:'([^']{2,80}?)'|\"([^\"]{2,80}?)\")(?=[\s,.;:)!?]|$)"
)

FRONTMATTER_KEY = re.compile(r"^([A-Za-z][\w-]*):\s*(.*)$")
SENTENCE_END = re.compile(r"(.*?[.!?])(?:\s|$)")


@dataclass(frozen=True)
class Surface:
    """One user- or model-reachable unit: a skill."""

    name: str
    kind: str  # "skill"
    slash_only: bool
    description: str
    path: Path
    commented_disable: bool = (
        False  # disable-model-invocation present but commented out
    )


# --- Parsing ------------------------------------------------------------------


def parse_frontmatter(text: str) -> dict[str, str]:
    """Parse the YAML-subset frontmatter this harness uses: plain scalars,
    quoted scalars, and '>' / '|' block scalars. Unknown lines are skipped."""
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        return {}
    fields: dict[str, str] = {}
    i = 1
    while i < len(lines):
        line = lines[i]
        if line.strip() == "---":
            break
        match = FRONTMATTER_KEY.match(line)
        if not match:
            i += 1
            continue
        key, value = match.group(1), match.group(2).strip()
        if value in {">", "|", ">-", "|-"}:
            block: list[str] = []
            i += 1
            while i < len(lines) and (
                lines[i].startswith((" ", "\t")) or not lines[i].strip()
            ):
                block.append(lines[i].strip())
                i += 1
            fields[key] = " ".join(part for part in block if part)
            continue
        if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
            value = value[1:-1]
        fields[key] = value
        i += 1
    return fields


def _frontmatter_block(text: str) -> str:
    """The raw lines between the opening and closing frontmatter fences."""
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        return ""
    block: list[str] = []
    for line in lines[1:]:
        if line.strip() == "---":
            break
        block.append(line)
    return "\n".join(block)


def load_skills(skills_dir: Path) -> list[Surface]:
    """Every .claude/skills/*/SKILL.md, classified by invocation mode. Only an
    uncommented `disable-model-invocation: true` key blocks model invocation —
    matching the loader's observed behaviour; a commented-out key is flagged
    (warning) but never changes classification."""
    surfaces: list[Surface] = []
    for skill_md in sorted(skills_dir.glob("*/SKILL.md")):
        raw = skill_md.read_text(encoding="utf-8")
        fields = parse_frontmatter(raw)
        block = _frontmatter_block(raw)
        surfaces.append(
            Surface(
                name=fields.get("name", skill_md.parent.name),
                kind="skill",
                slash_only=fields.get("disable-model-invocation") == "true",
                description=fields.get("description", ""),
                path=skill_md,
                commented_disable=(
                    "disable-model-invocation" in block
                    and "disable-model-invocation" not in fields
                ),
            )
        )
    return surfaces


def first_sentence(text: str) -> str:
    """First sentence of a description, whitespace-collapsed."""
    collapsed = " ".join(text.split())
    match = SENTENCE_END.match(collapsed)
    return match.group(1) if match else collapsed


# --- Rendering ----------------------------------------------------------------


def _cell(text: str) -> str:
    return text.replace("|", "\\|") if text else "(no description)"


def render_catalog(skills: list[Surface]) -> str:
    """The generated section: everything below the marker line. Model-invocable
    skills are names only — the model holds their descriptions natively; the
    slash table carries gists because the model never sees those descriptions."""
    model = sorted((s for s in skills if not s.slash_only), key=lambda s: s.name)
    slash = sorted((s for s in skills if s.slash_only), key=lambda s: s.name)
    lines = [
        "",
        (
            "<!-- Generated by the harness's skill-catalog.py --write (source harness only;"
            " plugin copies are regenerated at publish) — edit above the marker only. -->"
        ),
        "",
        "## Model-invocable skills",
        "",
        ", ".join(f"`{s.name}`" for s in model),
        "",
        "## Slash-only workflows",
        "",
        "| Surface | Use |",
        "|---|---|",
    ]
    lines += [
        f"| `/{s.name}` | {_cell(first_sentence(s.description))} |" for s in slash
    ]
    return "\n".join(lines) + "\n"


def split_at_marker(text: str) -> tuple[str, str] | None:
    """(hand-owned part incl. marker line, generated part), or None if the
    marker is missing."""
    lines = text.splitlines(keepends=True)
    for idx, line in enumerate(lines):
        if line.strip() == MARKER:
            return "".join(lines[: idx + 1]), "".join(lines[idx + 1 :])
    return None


# --- Linting ------------------------------------------------------------------


def shipped_text_files(surface: Surface) -> list[Path]:
    """Files linted for slash and `.claude/...` path references: every text
    file under a skill's folder (SKILL.md, references, scripts, assets)."""
    return sorted(
        candidate
        for candidate in surface.path.parent.rglob("*")
        if candidate.is_file() and candidate.suffix in SHIPPED_TEXT_SUFFIXES
    )


def slash_refs(path: Path) -> list[tuple[int, str]]:
    """(line number, token) for every slash reference in the file."""
    refs: list[tuple[int, str]] = []
    for lineno, line in enumerate(
        path.read_text(encoding="utf-8").splitlines(), start=1
    ):
        refs.extend((lineno, match.group(1)) for match in SLASH_REF.finditer(line))
    return refs


def claude_path_refs(path: Path) -> list[tuple[int, str]]:
    """(line number, token) for every checkable `.claude/` or `.repo-context/` token in
    the file. Placeholder/glob tokens (`<name>`, `*`, `{a,b}`, `…`, `...`) are
    not checkable and are omitted."""
    refs: list[tuple[int, str]] = []
    for lineno, line in enumerate(
        path.read_text(encoding="utf-8").splitlines(), start=1
    ):
        for match in CLAUDE_PATH.finditer(line):
            token = match.group(1).rstrip(".")
            if any(mark in token for mark in PLACEHOLDER_MARKS):
                continue
            refs.append((lineno, token))
    return refs


NEGATION_MARKERS = ("Never trigger", "Do NOT")


def _positive_span(description: str) -> str:
    """Description text up to the first negation marker. Phrases quoted after
    one ('Never trigger on X') are negative examples, not claims; positives
    after a negation clause are missed — acceptable for a warning-only check."""
    cut = len(description)
    for marker in NEGATION_MARKERS:
        idx = description.find(marker)
        if idx != -1:
            cut = min(cut, idx)
    return description[:cut]


def trigger_phrase_warnings(skills: list[Surface]) -> list[str]:
    """Quoted trigger phrases claimed by 2+ model-invocable descriptions."""
    claims: dict[str, set[str]] = {}
    for skill in skills:
        if skill.slash_only:
            continue
        for match in QUOTED_PHRASE.finditer(_positive_span(skill.description)):
            phrase = (match.group(1) or match.group(2)).lower()
            claims.setdefault(phrase, set()).add(skill.name)
    return [
        f"trigger phrase '{phrase}' claimed by {', '.join(sorted(names))}"
        for phrase, names in sorted(claims.items())
        if len(names) >= 2
    ]


def frontmatter_warnings(skills: list[Surface]) -> list[str]:
    """Skills whose frontmatter carries a commented-out disable-model-invocation."""
    return [
        f"{skill.name}: commented-out disable-model-invocation in frontmatter"
        " — the loader ignores it; uncomment or delete"
        for skill in sorted(skills, key=lambda s: s.name)
        if skill.commented_disable
    ]


# --- Codex twin of disable-model-invocation -----------------------------------

OPENAI_YAML_REL = Path("agents/openai.yaml")
POLICY_LINE = re.compile(
    r"^\s*allow_implicit_invocation:\s*(true|false)\s*$", re.MULTILINE
)


def expected_policy(surface: Surface) -> str:
    """Codex reads `policy.allow_implicit_invocation` from agents/openai.yaml,
    never Claude's frontmatter, so the value mirrors `disable-model-invocation`:
    a slash-only skill must not be auto-invoked on either tool."""
    return "false" if surface.slash_only else "true"


def render_openai_yaml(existing: str | None, policy: str) -> str:
    """The sidecar text: any existing `interface:` block kept verbatim (it is
    cosmetic, hand-owned), followed by the generated policy block."""
    interface_lines: list[str] = []
    if existing:
        in_interface = False
        for line in existing.splitlines():
            if line.startswith("interface:"):
                in_interface = True
            elif line and not line[0].isspace():
                in_interface = False
            if in_interface:
                interface_lines.append(line)
    body = "\n".join(
        interface_lines + ["policy:", f"  allow_implicit_invocation: {policy}"]
    )
    return body + "\n"


def openai_yaml_failures(skills: list[Surface]) -> list[str]:
    """Missing sidecar, or a policy that disagrees with the frontmatter flag."""
    failures: list[str] = []
    for surface in skills:
        sidecar = surface.path.parent / OPENAI_YAML_REL
        want = expected_policy(surface)
        if not sidecar.is_file():
            failures.append(
                f"missing {sidecar.parent.parent.name}/{OPENAI_YAML_REL} — run `{WRITE_CMD}`"
            )
            continue
        match = POLICY_LINE.search(sidecar.read_text(encoding="utf-8"))
        got = match.group(1) if match else None
        if got != want:
            failures.append(
                f"{surface.name}/{OPENAI_YAML_REL}: allow_implicit_invocation is {got},"
                f" frontmatter says {want} — run `{WRITE_CMD}`"
            )
    return failures


def write_openai_yamls(skills: list[Surface]) -> int:
    """Write/refresh every skill's sidecar; returns how many files changed."""
    changed = 0
    for surface in skills:
        sidecar = surface.path.parent / OPENAI_YAML_REL
        existing = sidecar.read_text(encoding="utf-8") if sidecar.is_file() else None
        rendered = render_openai_yaml(existing, expected_policy(surface))
        if rendered != existing:
            sidecar.parent.mkdir(parents=True, exist_ok=True)
            sidecar.write_text(rendered, encoding="utf-8")
            changed += 1
    return changed


# --- Commands -----------------------------------------------------------------


def _rel(path: Path, root: Path) -> str:
    try:
        return str(path.relative_to(root))
    except ValueError:
        return str(path)


def cmd_check(root: Path, skills_dir: Path) -> int:
    skills = load_skills(skills_dir)
    failures: list[str] = []
    warnings: list[str] = []

    known = {s.name for s in skills}
    router = skills_dir / ROUTER_REL
    if not router.is_file():
        failures.append(
            f"missing {_rel(router, root)} — create it with the marker, then run --write"
        )
    else:
        parts = split_at_marker(router.read_text(encoding="utf-8"))
        if parts is None:
            failures.append(f"marker {MARKER} missing from {_rel(router, root)}")
        else:
            # The hand-owned half names skills bare (`grilling`); a rename
            # would leave those stale while the generated tables stay green.
            for lineno, line in enumerate(parts[0].splitlines(), start=1):
                failures.extend(
                    f"unresolved skill name `{name}` at {_rel(router, root)}:{lineno}"
                    " (hand-owned section) — no installed skill"
                    for name in (m.group(1) for m in BARE_NAME.finditer(line))
                    if name not in known
                )
        if parts is not None and parts[1] != render_catalog(skills):
            diff = list(
                difflib.unified_diff(
                    parts[1].splitlines(),
                    render_catalog(skills).splitlines(),
                    fromfile="on-disk",
                    tofile="regenerated",
                    lineterm="",
                )
            )
            excerpt = "\n".join("    " + line for line in diff[:15])
            failures.append(f"stale generated section — run `{WRITE_CMD}`\n{excerpt}")

    used_allowlist: set[str] = set()
    used_path_allowlist: set[str] = set()
    resolved = 0
    resolved_paths = 0
    for surface in skills:
        for shipped in shipped_text_files(surface):
            for lineno, token in slash_refs(shipped):
                # `/mvp-plugin:adopt` is the plugin-namespaced form of `/adopt`:
                # resolve by the bare name so shipped skills may use either.
                bare = token[1:].split(":", 1)[-1]
                if bare in known:
                    resolved += 1
                elif token in ALLOWED_SLASHES:
                    used_allowlist.add(token)
                    resolved += 1
                else:
                    failures.append(
                        f"dead slash pointer {token} at {_rel(shipped, root)}:{lineno}"
                        " — no installed skill or allowlist entry"
                    )
            for lineno, token in claude_path_refs(shipped):
                if (root / token).exists():
                    resolved_paths += 1
                elif token in ALLOWED_PATHS:
                    used_path_allowlist.add(token)
                    resolved_paths += 1
                else:
                    failures.append(
                        f"dead path {token} at {_rel(shipped, root)}:{lineno}"
                        " — nothing on disk under the repo root, and no allowlist entry"
                    )
    warnings.extend(
        f"allowlist entry {token} ({ALLOWED_SLASHES[token]}) is no longer referenced — remove it"
        for token in sorted(set(ALLOWED_SLASHES) - used_allowlist)
    )
    warnings.extend(
        f"path allowlist entry {token} ({ALLOWED_PATHS[token]}) is no longer referenced — remove it"
        for token in sorted(set(ALLOWED_PATHS) - used_path_allowlist)
    )
    failures.extend(openai_yaml_failures(skills))
    warnings.extend(trigger_phrase_warnings(skills))
    warnings.extend(frontmatter_warnings(skills))

    model_count = sum(1 for s in skills if not s.slash_only)
    print(f"## skill-catalog check: {skills_dir}")
    print(
        f"{len(skills)} skills ({model_count} model-invocable, {len(skills) - model_count} slash-only)"
    )
    if not failures:
        print("OK   generated section is current")
        print(
            f"OK   {resolved} slash references resolve ({len(used_allowlist)} allowlisted)"
        )
        print(
            f"OK   {resolved_paths} shared path references resolve"
            f" ({len(used_path_allowlist)} allowlisted)"
        )
        print(f"OK   {len(skills)} agents/openai.yaml sidecars match their frontmatter")
    for warning in warnings:
        print(f"WARN {warning}")
    for failure in failures:
        print(f"FAIL {failure}")
    return 1 if failures else 0


def cmd_write(root: Path, skills_dir: Path) -> int:
    router = skills_dir / ROUTER_REL
    if not router.is_file():
        raise SystemExit(f"refusing: {_rel(router, root)} does not exist")
    parts = split_at_marker(router.read_text(encoding="utf-8"))
    if parts is None:
        raise SystemExit(
            f"refusing: marker {MARKER} missing from {_rel(router, root)}"
            " — everything above it is hand-owned; add the marker where the generated section starts"
        )
    skills = load_skills(skills_dir)
    updated = parts[0] + render_catalog(skills)
    if updated == parts[0] + parts[1]:
        print(f"already current: {_rel(router, root)}")
    else:
        router.write_text(updated, encoding="utf-8")
        print(f"regenerated: {_rel(router, root)}")
    changed = write_openai_yamls(skills)
    print(
        f"agents/openai.yaml sidecars: {changed} written, {len(skills) - changed} already current"
    )
    for warning in trigger_phrase_warnings(skills) + frontmatter_warnings(skills):
        print(f"WARN {warning}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Generate/lint the skill-router catalog."
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--check", action="store_true", help="verify catalog + slash pointers (default)"
    )
    mode.add_argument(
        "--write", action="store_true", help="regenerate the section below the marker"
    )
    parser.add_argument(
        "--root",
        type=Path,
        default=Path(__file__).resolve().parents[2],
        help="repo root containing .claude/ (default: derived from script location)",
    )
    parser.add_argument(
        "--skills-dir",
        type=Path,
        default=None,
        help="skills directory to catalog/lint (default: <root>/.claude/skills). The publish"
        " step points this at a plugin's skills/ so its shipped router lists only shipped skills;"
        " `.claude/...` path tokens still resolve against --root",
    )
    args = parser.parse_args(argv)
    root = args.root.resolve()
    skills_dir = (args.skills_dir or root / ".claude" / "skills").resolve()
    if not skills_dir.is_dir():
        raise SystemExit(
            f"no skills directory at {skills_dir} — pass --root or --skills-dir"
        )
    return cmd_write(root, skills_dir) if args.write else cmd_check(root, skills_dir)


if __name__ == "__main__":
    raise SystemExit(main())
