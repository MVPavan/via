#!/usr/bin/env python3
"""Fail when Core or CLI production code names a harness (adapters design §5.6).

This guards against harness literals only; it cannot prove that Core has no
harness-shaped behaviour, which review still owns.
"""

import re
import sys
import tempfile
from pathlib import Path

SCOPES = ("crates/via-core/src", "crates/via-cli/src")
HARNESS_TABLE = "crates/via-adapters/src/harness.rs"
ALLOW_FILE = "scripts/harness-literals-allow.txt"
EXTRA_NAMES = ("fake", "acp", "anthropic", "openai")


class GuardError(Exception):
    """A configuration problem that stops the guard before it scans."""


IDENT_START = re.compile(r"[A-Za-z_]")
IDENT = re.compile(r"[A-Za-z0-9_]*")
WORD = re.compile(r"[A-Za-z0-9_]+")
SUBWORD = re.compile(r"[A-Z]+(?![a-z])|[A-Z]?[a-z]+")
RAW_STRING = re.compile(r'(?:b|c)?r(#*)"')
CHAR_LITERAL = re.compile(r"'(?:\\(?:u\{[0-9A-Fa-f_]*\}|x[0-9A-Fa-f]{2}|.)|[^\\'\n])'")
ITEM_KEYWORDS = {"fn", "struct", "enum", "union", "impl", "trait", "type", "mod"}
OPEN = {"(": ")", "[": "]", "{": "}"}


class Token:
    """One lexed token: `kind` is ident, str, comment, punct or other."""

    def __init__(self, kind, text, line):
        self.kind = kind
        self.text = text
        self.line = line


def lex(source):
    """Split Rust source into tokens, keeping literal and comment contents whole."""
    tokens = []
    i = 0
    line = 1
    length = len(source)

    def push(kind, end):
        nonlocal i, line
        text = source[i:end]
        tokens.append(Token(kind, text, line))
        line += text.count("\n")
        i = end

    while i < length:
        ch = source[i]
        if ch.isspace():
            if ch == "\n":
                line += 1
            i += 1
        elif source.startswith("//", i):
            end = source.find("\n", i)
            push("comment", length if end < 0 else end)
        elif source.startswith("/*", i):
            depth = 0
            j = i
            while j < length:
                if source.startswith("/*", j):
                    depth += 1
                    j += 2
                elif source.startswith("*/", j):
                    depth -= 1
                    j += 2
                    if depth == 0:
                        break
                else:
                    j += 1
            push("comment", j)
        elif (raw := RAW_STRING.match(source, i)) is not None:
            close = '"' + raw.group(1)
            end = source.find(close, raw.end())
            push("str", length if end < 0 else end + len(close))
        elif ch == '"' or (ch in "bc" and source.startswith('"', i + 1)):
            j = source.index('"', i) + 1
            while j < length and source[j] != '"':
                j += 2 if source[j] == "\\" else 1
            push("str", min(j + 1, length))
        elif ch == "'" or (ch == "b" and source.startswith("'", i + 1)):
            start = i + 1 if ch == "b" else i
            literal = CHAR_LITERAL.match(source, start)
            if literal is not None:
                push("other", literal.end())
            else:
                # A lifetime or label: its name is an identifier.
                i = start + 1
                word = IDENT.match(source, i)
                push("ident", word.end())
        elif source.startswith("r#", i) and IDENT_START.match(source, i + 2):
            i += 2
            push("ident", IDENT.match(source, i).end())
        elif IDENT_START.match(ch):
            push("ident", IDENT.match(source, i).end())
        elif ch.isdigit():
            push("other", IDENT.match(source, i).end())
        else:
            push("punct", i + 1)
    return tokens


def subwords(word):
    """Split on `_`, digits and camelCase boundaries, then lowercase."""
    return [part.lower() for part in SUBWORD.findall(word)]


def words(token):
    """Yield `(line, word)` for each identifier, string or comment word."""
    if token.kind == "ident":
        yield token.line, token.text
    elif token.kind in ("str", "comment"):
        for match in WORD.finditer(token.text):
            yield token.line + token.text.count("\n", 0, match.start()), match.group()


def matched_names(parts, names):
    """Names whose joined form equals a contiguous run of subwords."""
    found = set()
    for start in range(len(parts)):
        joined = ""
        for part in parts[start:]:
            joined += part
            if joined in names:
                found.add(joined)
            if len(joined) >= max(map(len, names)):
                break
    return found


def closing(tokens, index):
    """Index of the bracket closing the one at `index`."""
    depth = 0
    for j in range(index, len(tokens)):
        token = tokens[j]
        if token.kind != "punct":
            continue
        if token.text in OPEN:
            depth += 1
        elif token.text in ")]}":
            depth -= 1
            if depth == 0:
                return j
    return len(tokens) - 1


def attribute_end(tokens, index):
    """If an outer attribute starts at `index`, return its closing `]` index."""
    if (
        tokens[index].text == "#"
        and index + 1 < len(tokens)
        and tokens[index + 1].text == "["
    ):
        return closing(tokens, index + 1)
    return None


def item_end(tokens, index):
    """Last token index of the item starting at `index`.

    It ends at `;`, at its first top-level brace block's match, or at a
    top-level `,` when it is a field, argument or arm rather than a
    declaration. A closing bracket of the enclosing block ends it too.
    """
    declaration = False
    j = index
    while j < len(tokens):
        token = tokens[j]
        if token.kind == "ident" and token.text in ITEM_KEYWORDS:
            declaration = True
        elif token.kind == "punct":
            if token.text == ";":
                return j
            if token.text == "," and not declaration:
                return j
            if token.text == "{":
                return closing(tokens, j)
            if token.text in "([":
                j = closing(tokens, j)
            elif token.text in ")]}":
                return j - 1
        j += 1
    return len(tokens) - 1


def test_regions(tokens):
    """Token index ranges `(start, end)` of items under `#[cfg(test)]`."""
    regions = []
    i = 0
    while i < len(tokens):
        end = attribute_end(tokens, i)
        if end is None:
            i += 1
            continue
        attribute = "".join(token.text for token in tokens[i + 2 : end])
        if attribute != "cfg(test)":
            i = end + 1
            continue
        j = end + 1
        while j < len(tokens):
            if tokens[j].kind == "comment":
                j += 1
            elif (next_end := attribute_end(tokens, j)) is not None:
                j = next_end + 1
            else:
                break
        last = item_end(tokens, j) if j < len(tokens) else j - 1
        regions.append((i, last))
        i = last + 1
    return regions


def module_declarations(tokens, regions):
    """Map each `mod name;` to whether every declaration of it is test-only."""
    declared = {}
    for i in range(len(tokens) - 2):
        if (
            tokens[i].kind == "ident"
            and tokens[i].text == "mod"
            and tokens[i + 1].kind == "ident"
            and tokens[i + 2].text == ";"
        ):
            test_only = any(start <= i <= end for start, end in regions)
            name = tokens[i + 1].text
            declared[name] = declared.get(name, True) and test_only
    return declared


def parent_candidates(path, scope):
    """Files that could declare the module stored at `path`."""
    if path.name == "mod.rs":
        name, directory = path.parent.name, path.parent.parent
    else:
        name, directory = path.stem, path.parent
    candidates = [directory / "mod.rs"]
    if directory == scope:
        candidates += [scope / "lib.rs", scope / "main.rs"]
    else:
        candidates.append(directory.parent / f"{directory.name}.rs")
    return name, candidates


def load_names(root):
    """Normalized forbidden names from the HARNESSES table plus the fixed extras."""
    table = root / HARNESS_TABLE
    if not table.is_file():
        raise GuardError(f"{HARNESS_TABLE} is missing; it must define the HARNESSES table")
    tokens = lex(table.read_text())
    names = []
    for i, token in enumerate(tokens):
        if token.kind != "ident" or token.text != "HARNESSES":
            continue
        j = i
        while j < len(tokens) and tokens[j].text not in ("=", ";"):
            j += 1
        if j >= len(tokens) or tokens[j].text != "=":
            continue
        end = item_end(tokens, j)
        for k in range(j, end - 1):
            if (
                tokens[k].text == "name"
                and tokens[k + 1].text == ":"
                and tokens[k + 2].kind == "str"
            ):
                names.append(tokens[k + 2].text.strip('"'))
        break
    if not names:
        raise GuardError(f"{HARNESS_TABLE} has no HARNESSES table with `name: \"...\"` rows")
    return {"".join(subwords(name)) for name in [*names, *EXTRA_NAMES]}


def load_allow(root):
    """`(path, substring)` pairs from the allow file."""
    allowed = []
    path = root / ALLOW_FILE
    if not path.is_file():
        return allowed
    for number, line in enumerate(path.read_text().splitlines(), 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        parts = line.split(": ", 2)
        if len(parts) != 3 or not all(part.strip() for part in parts):
            raise GuardError(f"{ALLOW_FILE}:{number}: expected `path: substring: reason`")
        allowed.append((parts[0].strip(), parts[1]))
    return allowed


def check(root):
    """Return the sorted findings under `root` as `path:line: name: text` lines."""
    names = load_names(root)
    allowed = load_allow(root)
    findings = []
    for scope_name in SCOPES:
        scope = root / scope_name
        files = sorted(scope.rglob("*.rs"))
        lexed = {path: lex(path.read_text()) for path in files}
        regions = {path: test_regions(tokens) for path, tokens in lexed.items()}
        declarations = {path: module_declarations(lexed[path], regions[path]) for path in files}
        excluded = {}

        def is_excluded(path):
            """True when `path` is reached only through test-only declarations."""
            if path not in excluded:
                excluded[path] = False
                name, candidates = parent_candidates(path, scope)
                parents = [p for p in candidates if name in declarations.get(p, {})]
                excluded[path] = bool(parents) and all(
                    declarations[p][name] or is_excluded(p) for p in parents
                )
            return excluded[path]

        for path in files:
            if is_excluded(path):
                continue
            relative = path.relative_to(root).as_posix()
            lines = path.read_text().splitlines()
            skipped = regions[path]
            hits = set()
            for index, token in enumerate(lexed[path]):
                if any(start <= index <= end for start, end in skipped):
                    continue
                for line, word in words(token):
                    for name in matched_names(subwords(word), names):
                        hits.add((line, name))
            for line, name in sorted(hits):
                text = lines[line - 1].strip()
                if any(relative == a_path and sub in text for a_path, sub in allowed):
                    continue
                findings.append(f"{relative}:{line}: {name}: {text}")
    return findings


# --- self-test ---------------------------------------------------------------

SELF_TEST_TABLE = """\
pub struct HarnessRow { pub name: &'static str, pub route: &'static str, pub default_binary: &'static str }
pub const HARNESSES: &[HarnessRow] = &[
    HarnessRow { name: "claude", route: "claude-cli", default_binary: "claude" },
    HarnessRow { name: "codex", route: "codex-app-server", default_binary: "codex" },
    HarnessRow { name: "opencode", route: "opencode-serve", default_binary: "opencode" },
];
"""

SELF_TEST_FILES = {
    "crates/via-core/src/lib.rs": """\
//! Crate root.
mod engine;
#[cfg(test)]
mod tests;
mod other;

pub struct FakeConfig;
fn fake_cwd() {}
const NAME: &str = "codex";
struct OpenCodeAdapter;
// OpenAI is a vendor name.
/* the claude harness */
const RAW: &str = r##"
}
"##;
fn clean() -> u32 { 1 }

#[cfg(test)]
mod inline_tests {
    const RAW: &str = r#"
}
"#;
    fn fake_helper() {}
}

pub struct Engine {
    #[cfg(test)]
    faults: FakeFaults,
    open: u32,
}
""",
    "crates/via-core/src/tests.rs": """\
fn fake() {}
""",
    "crates/via-core/src/other.rs": """\
mod tests;
""",
    "crates/via-core/src/other/tests.rs": """\
fn anthropic_key() {}
""",
    "crates/via-cli/src/main.rs": """\
fn main() { let x = 'a'; let _l: &'static str = "allowed codex"; }
""",
}

SELF_TEST_ALLOW = """\
# path: substring: reason
crates/via-cli/src/main.rs: allowed codex: self-test allow entry
"""

SELF_TEST_EXPECTED = [
    "crates/via-core/src/lib.rs:7: fake: pub struct FakeConfig;",
    "crates/via-core/src/lib.rs:8: fake: fn fake_cwd() {}",
    'crates/via-core/src/lib.rs:9: codex: const NAME: &str = "codex";',
    "crates/via-core/src/lib.rs:10: opencode: struct OpenCodeAdapter;",
    "crates/via-core/src/lib.rs:11: openai: // OpenAI is a vendor name.",
    "crates/via-core/src/lib.rs:12: claude: /* the claude harness */",
    "crates/via-core/src/other/tests.rs:1: anthropic: fn anthropic_key() {}",
]


def write_tree(root, files, table, allow):
    """Write a fixture tree; `table` or `allow` of None leaves that file out."""
    for path, text in files.items():
        target = root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)
    if table is not None:
        (root / HARNESS_TABLE).parent.mkdir(parents=True, exist_ok=True)
        (root / HARNESS_TABLE).write_text(table)
    if allow is not None:
        (root / ALLOW_FILE).parent.mkdir(parents=True, exist_ok=True)
        (root / ALLOW_FILE).write_text(allow)


def self_test():
    failures = []
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp) / "full"
        write_tree(root, SELF_TEST_FILES, SELF_TEST_TABLE, SELF_TEST_ALLOW)
        found = check(root)
        if found != SELF_TEST_EXPECTED:
            failures.append(
                "findings differ\n  expected:\n    "
                + "\n    ".join(SELF_TEST_EXPECTED)
                + "\n  actual:\n    "
                + "\n    ".join(found)
            )
        for case, table in (("missing harness.rs", None), ("missing table", "pub const X: u8 = 0;\n")):
            root = Path(tmp) / case.replace(" ", "-")
            write_tree(root, SELF_TEST_FILES, table, SELF_TEST_ALLOW)
            try:
                check(root)
                failures.append(f"{case}: no error")
            except GuardError:
                pass
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    print("self-test:", "FAILED" if failures else "ok")
    return 1 if failures else 0


def main(argv):
    if argv[1:] == ["--self-test"]:
        return self_test()
    if argv[1:]:
        print("usage: check-harness-literals.py [--self-test]", file=sys.stderr)
        return 2
    try:
        findings = check(Path(__file__).resolve().parents[1])
    except GuardError as error:
        print(f"check-harness-literals: {error}", file=sys.stderr)
        return 2
    for finding in findings:
        print(finding)
    if findings:
        print(f"{len(findings)} harness literal(s) in Core or CLI production code", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
