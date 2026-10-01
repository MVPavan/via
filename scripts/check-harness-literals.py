#!/usr/bin/env python3
"""Fail when Core or CLI production code names a harness (adapters design §5.6).

This guards against harness literals only; it cannot prove that Core has no
harness-shaped behaviour, which review still owns.

Its one hard rule: it may report too much, but it must never hide a
production literal. Wherever it cannot cheaply establish that code is
test-only, it scans that code. It is not a Rust module resolver.
"""

import bisect
import re
import sys
import tempfile
from pathlib import Path

SCOPES = ("crates/via-core/src", "crates/via-cli/src")
HARNESS_TABLE = "crates/via-adapters/src/harness.rs"
ALLOW_FILE = "scripts/harness-literals-allow.txt"
EXTRA_NAMES = ("fake", "acp", "anthropic", "openai")
TABLE_NAME = re.compile(r'"[A-Za-z0-9_-]+"')


class GuardError(Exception):
    """A configuration problem that stops the guard before it scans."""


# --- lexer -------------------------------------------------------------------

IDENT_START = re.compile(r"[A-Za-z_]")
IDENT = re.compile(r"[A-Za-z0-9_]*")
WORD = re.compile(r"[A-Za-z0-9_]+")
SUBWORD = re.compile(r"[A-Z]+(?![a-z])|[A-Z]?[a-z]+")
RAW_STRING = re.compile(r'(?:b|c)?r(#*)"')
CHAR_LITERAL = re.compile(r"'(?:\\(?:u\{[0-9A-Fa-f_]*\}|x[0-9A-Fa-f]{2}|.)|[^\\'\n])'")
OPEN = {"(": ")", "[": "]", "{": "}"}


class Token:
    """One lexed token: `kind` is ident, str, comment, punct or other."""

    def __init__(self, kind, text, offset):
        self.kind = kind
        self.text = text
        self.offset = offset


def lex(source):
    """Split Rust source into tokens, keeping literal and comment contents whole."""
    tokens = []
    i = 0
    length = len(source)

    def push(kind, end, start=None):
        nonlocal i
        start = i if start is None else start
        tokens.append(Token(kind, source[start:end], start))
        i = end

    while i < length:
        ch = source[i]
        if ch.isspace():
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
                push("ident", IDENT.match(source, start + 1).end(), start + 1)
        elif source.startswith("r#", i) and IDENT_START.match(source, i + 2):
            # The `r#` prefix stays, so a raw `r#mod` is never the keyword.
            push("ident", IDENT.match(source, i + 2).end())
        elif IDENT_START.match(ch):
            push("ident", IDENT.match(source, i).end())
        elif ch.isdigit():
            push("other", IDENT.match(source, i).end())
        else:
            push("punct", i + 1)
    return tokens


def is_doc(token):
    """True for an outer doc comment (`///` or `/** */`)."""
    text = token.text
    if token.kind != "comment":
        return False
    if text.startswith("///"):
        return not text.startswith("////")
    return text.startswith("/**") and not text.startswith("/***") and text != "/**/"


# --- syntax helpers (comments are skipped everywhere) --------------------------


def code_at(tokens, index):
    """Index of the first non-comment token at or after `index`, or len(tokens)."""
    while index < len(tokens) and tokens[index].kind == "comment":
        index += 1
    return index


def text_at(tokens, index):
    return tokens[index].text if index < len(tokens) else None


def closing(tokens, index):
    """Index of the bracket closing the one at `index`, or None if unmatched."""
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
                return j if OPEN[tokens[index].text] == token.text else None
    return None


def attribute_end(tokens, index):
    """If an outer attribute starts at `index`, return its closing `]` index."""
    if text_at(tokens, index) != "#" or tokens[index].kind != "punct":
        return None
    bracket = code_at(tokens, index + 1)
    if text_at(tokens, bracket) != "[":
        return None
    return closing(tokens, bracket)


def attribute_text(tokens, index, end):
    """An attribute's code with comments and whitespace removed."""
    return "".join(t.text for t in tokens[index:end] if t.kind != "comment")


def item_end(tokens, index):
    """Last token index of the item starting at `index`, or None.

    Only items at item level count: an optional visibility and qualifiers,
    then `mod`, `fn`, `impl`, `struct`, `enum`, `trait`, `use`, `const`,
    `static`, `type` or `macro_rules!`. A braced item ends at its body's
    matching brace or at a `;` at depth 0, with `<>` tracked in its header;
    the others end at a `;` at depth 0. Anything else, or anything that
    cannot be established, is None, so it is scanned.
    """
    k = code_at(tokens, index)
    if text_at(tokens, k) == "pub":
        k = code_at(tokens, k + 1)
        if text_at(tokens, k) == "(":
            end = closing(tokens, k)
            if end is None:
                return None
            k = code_at(tokens, end + 1)
    while True:
        word = text_at(tokens, k)
        after = code_at(tokens, k + 1)
        if word in ("async", "unsafe", "default", "safe"):
            k = after
        elif word == "extern":
            k = after
            if k < len(tokens) and tokens[k].kind == "str":
                k = code_at(tokens, k + 1)
        elif word == "const" and text_at(tokens, after) in ("fn", "unsafe", "async", "extern"):
            k = after
        else:
            break
    word = text_at(tokens, k)
    if word is None or tokens[k].kind != "ident":
        return None
    if word in ("const", "static"):
        name = code_at(tokens, k + 1)
        if text_at(tokens, name) == "mut" and word == "static":
            name = code_at(tokens, name + 1)
        if name >= len(tokens) or tokens[name].kind != "ident":
            return None
        if text_at(tokens, code_at(tokens, name + 1)) != ":":
            return None
        return semicolon_end(tokens, k + 1)
    if word in ("use", "type"):
        return semicolon_end(tokens, k + 1)
    if word == "macro_rules":
        if text_at(tokens, code_at(tokens, k + 1)) != "!":
            return None
        return braced_end(tokens, k + 1)
    if word in ("mod", "fn", "impl", "struct", "enum", "trait"):
        return braced_end(tokens, k + 1)
    return None


def semicolon_end(tokens, index):
    """The `;` ending an item at bracket depth 0, or None."""
    j = code_at(tokens, index)
    while j < len(tokens):
        token = tokens[j]
        if token.kind == "punct":
            if token.text in OPEN:
                end = closing(tokens, j)
                if end is None:
                    return None
                j = code_at(tokens, end + 1)
                continue
            if token.text == ";":
                return j
            if token.text in ")]}":
                return None
        j = code_at(tokens, j + 1)
    return None


def braced_end(tokens, index):
    """The end of a braced item's body, or its `;`, at depth 0; or None."""
    angle = 0
    previous = None
    j = code_at(tokens, index)
    while j < len(tokens):
        token = tokens[j]
        if token.kind == "punct":
            if token.text in OPEN:
                end = closing(tokens, j)
                if end is None:
                    return None
                if token.text == "{" and angle == 0:
                    return end
                previous = tokens[end]
                j = code_at(tokens, end + 1)
                continue
            if token.text == ";" and angle == 0:
                return j
            if token.text in ")]}":
                return None
            if token.text == "<":
                angle += 1
            elif token.text == ">":
                arrow = (
                    previous is not None
                    and previous.text in "-="
                    and previous.offset + 1 == token.offset
                )
                if not arrow:
                    angle -= 1
                    if angle < 0:
                        return None
        previous = token
        j = code_at(tokens, j + 1)
    return None


def macro_input_end(tokens, index):
    """If a macro invocation `name!(…)`, `name![…]`, `name!{…}` or a
    `macro_rules! name {…}` starts at `index`, the index of its input's
    closing bracket (the last token if unmatched); otherwise None."""
    if tokens[index].kind != "ident":
        return None
    bang = code_at(tokens, index + 1)
    if text_at(tokens, bang) != "!" or tokens[bang].kind != "punct":
        return None
    opening = code_at(tokens, bang + 1)
    if tokens[index].text == "macro_rules" and opening < len(tokens):
        if tokens[opening].kind == "ident":
            opening = code_at(tokens, opening + 1)
    if text_at(tokens, opening) not in OPEN:
        return None
    end = closing(tokens, opening)
    return len(tokens) - 1 if end is None else end


def test_regions(tokens):
    """Token index ranges `(start, end)` of items under `#[cfg(test)]`.

    A region starts at the item's attribute and doc-comment cluster. Only
    the exact attribute `cfg(test)` counts; composite forms are scanned.
    Nothing inside a macro invocation's input is excluded, since the macro
    may emit it as production code; a `#[cfg(test)] macro_rules!` item is
    still excluded as an item.
    """
    regions = []
    i = 0
    while i < len(tokens):
        macro_end = macro_input_end(tokens, i)
        if macro_end is not None:
            i = macro_end + 1
            continue
        if not (is_doc(tokens[i]) or attribute_end(tokens, i) is not None):
            i += 1
            continue
        start = i
        test = False
        j = i
        while j < len(tokens):
            if tokens[j].kind == "comment":
                j += 1
                continue
            end = attribute_end(tokens, j)
            if end is None:
                break
            if attribute_text(tokens, code_at(tokens, j + 1) + 1, end) == "cfg(test)":
                test = True
            j = end + 1
        last = item_end(tokens, j) if test else None
        if last is not None:
            regions.append((start, last))
            i = last + 1
        else:
            i = max(j, i + 1)
    return regions


def in_regions(index, regions):
    return any(start <= index <= end for start, end in regions)


# --- names and tokens --------------------------------------------------------


def subwords(word):
    """Split on `_`, digits and camelCase boundaries, then lowercase."""
    return [part.lower() for part in SUBWORD.findall(word)]


def words(token):
    """Yield `(offset, word)` for each identifier, string or comment word."""
    if token.kind == "ident":
        prefix = 2 if token.text.startswith("r#") else 0
        yield token.offset + prefix, token.text[prefix:]
    elif token.kind in ("str", "comment"):
        for match in WORD.finditer(token.text):
            yield token.offset + match.start(), match.group()


def matched_names(parts, names):
    """Names whose joined form equals a contiguous run of subwords."""
    longest = max(map(len, names))
    found = set()
    for start in range(len(parts)):
        joined = ""
        for part in parts[start:]:
            joined += part
            if joined in names:
                found.add(joined)
            if len(joined) >= longest:
                break
    return found


def load_names(root):
    """Normalized forbidden names from the HARNESSES table plus the fixed extras."""
    table = root / HARNESS_TABLE
    if not table.is_file():
        raise GuardError(f"{HARNESS_TABLE} is missing; it must define the HARNESSES table")
    code = [t for t in lex(table.read_text()) if t.kind != "comment"]
    starts = [
        i
        for i in range(len(code) - 2)
        if [t.text for t in code[i : i + 3]] == ["pub", "const", "HARNESSES"]
    ]
    if len(starts) != 1:
        raise GuardError(
            f"{HARNESS_TABLE}: expected one `pub const HARNESSES`, found {len(starts)}"
        )
    end = semicolon_end(code, starts[0] + 3)
    equals = next(
        (i for i in range(starts[0] + 3, end or 0) if code[i].text == "="), None
    )
    if end is None or equals is None:
        raise GuardError(f"{HARNESS_TABLE}: HARNESSES has no initializer ending in `;`")
    body = code[equals:end]
    names = []
    rows = 0
    for k, token in enumerate(body):
        following = [t.text for t in body[k + 1 : k + 3]]
        if token.text == "HarnessRow" and following[:1] == ["{"]:
            rows += 1
        if token.text == "name" and following[:1] == [":"] and k + 2 < len(body):
            value = body[k + 2]
            if value.kind != "str" or not TABLE_NAME.fullmatch(value.text):
                raise GuardError(f"{HARNESS_TABLE}: a row's name is not a plain string literal")
            names.append(value.text.strip('"'))
    if not names or len(names) != rows:
        raise GuardError(
            f"{HARNESS_TABLE}: found {len(names)} `name: \"...\"` in {rows} HarnessRow rows"
        )
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


# --- file exclusion ----------------------------------------------------------


def include_arguments(code):
    """The argument tokens of each `include!` in comment-free `code`."""
    for k, token in enumerate(code):
        if token.text == "include" and k + 2 < len(code) and code[k + 1].text == "!":
            end = closing(code, k + 2) if code[k + 2].text in OPEN else None
            yield code[k + 3 : end] if end is not None else None


def path_mentions(lexed):
    """Strings in `#[path = ...]` attributes and `include!` arguments."""
    mentions = []
    for tokens in lexed.values():
        code = [t for t in tokens if t.kind != "comment"]
        for k, token in enumerate(code):
            if token.text == "path" and k + 2 < len(code) and code[k + 1].text == "=":
                if code[k + 2].kind == "str":
                    mentions.append(code[k + 2].text)
        for argument in include_arguments(code):
            mentions.extend(t.text for t in argument or [] if t.kind == "str")
    return mentions


def computed_include(lexed):
    """True if any `include!` argument is not exactly one plain string literal."""
    for tokens in lexed.values():
        code = [t for t in tokens if t.kind != "comment"]
        for argument in include_arguments(code):
            plain = (
                argument is not None
                and len(argument) == 1
                and argument[0].kind == "str"
                and argument[0].text.startswith('"')
            )
            if not plain:
                return True
    return False


def module_declarations(tokens, name):
    """`(index, top_level)` of each `mod name` declaration in `tokens`."""
    found = []
    depth = 0
    for i, token in enumerate(tokens):
        if token.kind == "comment":
            continue
        if token.kind == "punct" and token.text == "{":
            depth += 1
        elif token.kind == "punct" and token.text == "}":
            depth -= 1
        elif token.text == "mod":
            target = code_at(tokens, i + 1)
            if text_at(tokens, target) in (name, f"r#{name}"):
                after = text_at(tokens, code_at(tokens, target + 1))
                if after in (";", "{"):
                    found.append((i, depth == 0 and after == ";"))
    return found


def excluded_files(scope, lexed, regions, mentions):
    """Files reached only through one top-level `#[cfg(test)] mod name;`.

    Deliberately conservative: the file is excluded only when the crate
    holds exactly one `mod name` declaration (inline or not, at any depth,
    in any of its files), and that one is a top-level, test-only
    `mod name;` in the file's standard-layout declaring file; no `#[path]`
    or `include!` names it; no `include!` in the crate has a computed
    argument; and it is not a crate root. Anything else is scanned.
    """
    excluded = set()
    if computed_include(lexed):
        return excluded
    for path in lexed:
        relative = path.relative_to(scope)
        if path.name in ("lib.rs", "main.rs") and path.parent == scope:
            continue
        if relative.parts[0] == "bin":
            continue
        if mentions is None or any(path.name in mention for mention in mentions):
            continue
        if path.name == "mod.rs":
            name, directory = path.parent.name, path.parent.parent
        else:
            name, directory = path.stem, path.parent
        if directory == scope:
            parents = [scope / "lib.rs", scope / "main.rs"]
        else:
            parents = [directory.parent / f"{directory.name}.rs", directory / "mod.rs"]
        declarations = [
            (declaring, index, top)
            for declaring in lexed
            for index, top in module_declarations(lexed[declaring], name)
        ]
        if len(declarations) == 1:
            declaring, index, top = declarations[0]
            if declaring in parents and top and in_regions(index, regions[declaring]):
                excluded.add(path)
    return excluded


# --- check -------------------------------------------------------------------


def check(root):
    """Return the findings under `root` as `path:line: name: text` lines."""
    names = load_names(root)
    allowed = load_allow(root)
    sources = {}
    lexed = {}
    for scope_name in SCOPES:
        for path in sorted((root / scope_name).rglob("*.rs")):
            sources[path] = path.read_text()
            lexed[path] = lex(sources[path])
    regions = {path: test_regions(tokens) for path, tokens in lexed.items()}
    mentions = path_mentions(lexed)
    findings = []
    for scope_name in SCOPES:
        scope = root / scope_name
        in_scope = {p: t for p, t in lexed.items() if p.is_relative_to(scope)}
        excluded = excluded_files(scope, in_scope, regions, mentions)
        for path in in_scope:
            if path not in excluded:
                findings.extend(
                    scan(root, path, sources[path], lexed[path], regions[path], names, allowed)
                )
    return findings


def scan(root, path, source, tokens, regions, names, allowed):
    """Findings in one file, outside its test regions and allow entries."""
    relative = path.relative_to(root).as_posix()
    line_starts = [0] + [i + 1 for i, ch in enumerate(source) if ch == "\n"]
    lines = source.split("\n")
    spans = {}
    for index, token in enumerate(tokens):
        if in_regions(index, regions):
            continue
        for offset, word in words(token):
            for name in matched_names(subwords(word), names):
                line = bisect.bisect_right(line_starts, offset)
                column = offset - line_starts[line - 1]
                spans.setdefault((line, name), []).append((column, column + len(word)))
    entries = [sub for a_path, sub in allowed if a_path == relative]
    findings = []
    for (line, name), found in sorted(spans.items()):
        text = lines[line - 1]
        covers = [
            (m.start(), m.end()) for sub in entries for m in re.finditer(re.escape(sub), text)
        ]
        if all(any(s <= a and b <= e for s, e in covers) for a, b in found):
            continue
        findings.append(f"{relative}:{line}: {name}: {text.strip()}")
    return findings


# --- self-test ---------------------------------------------------------------
#
# Each case is an independent fixture tree. `expect` lists `(path, marker,
# name)`: the finding on the one line of `path` containing `marker`. Every
# finding not listed fails the case, so each case is positive and negative.

CORE = "crates/via-core/src/"
CLI = "crates/via-cli/src/"

TABLE = """\
pub struct HarnessRow { pub name: &'static str, pub route: &'static str, pub default_binary: &'static str }
pub const HARNESSES: &[HarnessRow] = &[
    HarnessRow { name: "claude", route: "claude-cli", default_binary: "claude" },
    HarnessRow { name: "codex", route: "codex-app-server", default_binary: "codex" },
    HarnessRow { name: "opencode", route: "opencode-serve", default_binary: "opencode" },
];
"""

SENTINEL_TABLE = """\
pub struct HarnessRow { pub name: &'static str, pub route: &'static str, pub default_binary: &'static str }
pub fn rows() -> &'static [HarnessRow] { HARNESSES }
pub const OTHER: HarnessRow = HarnessRow { name: "wrong", route: "w", default_binary: "w" };
pub const HARNESSES: &[HarnessRow] = &[
    HarnessRow { name /* canonical */: "zeta", route: "z", default_binary: "z" },
    HarnessRow { name: "omega", route: "o", default_binary: "o" },
];
"""

CASES = [
    {
        "name": "named examples and subwords",
        "files": {
            CORE + "lib.rs": """\
pub struct FakeConfig;
fn fake_cwd() {}
const NAME: &str = "codex";
struct OpenCodeAdapter;
// OpenAI is a vendor.
/* the claude harness */
fn clean(open: u8, code: u8) -> u8 { open + code }
struct Codexes; struct Fakery;
""",
            CLI + "main.rs": "fn main() {}\n",
        },
        "expect": [
            (CORE + "lib.rs", "FakeConfig", "fake"),
            (CORE + "lib.rs", "fake_cwd", "fake"),
            (CORE + "lib.rs", '"codex"', "codex"),
            (CORE + "lib.rs", "OpenCodeAdapter", "opencode"),
            (CORE + "lib.rs", "OpenAI", "openai"),
            (CORE + "lib.rs", "claude harness", "claude"),
        ],
    },
    {
        "name": "raw strings",
        "files": {
            CORE + "lib.rs": """\
const A: &str = r#"codex"#;
const B: &[u8] = br##"say "claude" }"##;
const Q: &str = r#"a " b"#;
fn codex_after_quote() {}
const Z: &str = "z";
#[cfg(test)]
mod raw_tests {
    const S: &str = r"C:\\";
    const R: &str = r#"
}
"quoted" fake
"#;
    fn fake_in_test() {}
    const T: &str = "x";
}
fn after_raw_openai() {}
""",
        },
        "expect": [
            (CORE + "lib.rs", "const A", "codex"),
            (CORE + "lib.rs", "const B", "claude"),
            (CORE + "lib.rs", "codex_after_quote", "codex"),
            (CORE + "lib.rs", "after_raw_openai", "openai"),
        ],
    },
    {
        "name": "chars and lifetimes",
        "files": {
            CORE + "lib.rs": """\
const QUOTE: char = '"';
fn codex_after_char() {}
fn life<'a>(codex_param: &'a str) -> &'a str { todo() }
fn lifetime_named<'fake>() {}
#[cfg(test)]
mod char_tests {
    const C: char = '}';
    fn fake_char() {}
}
fn after_char_mod_claude() {}
const E: u8 = b'\\'';
fn after_byte_char_codex() {}
""",
        },
        "expect": [
            (CORE + "lib.rs", "codex_after_char", "codex"),
            (CORE + "lib.rs", "fn life<", "codex"),
            (CORE + "lib.rs", "lifetime_named", "fake"),
            (CORE + "lib.rs", "after_char_mod_claude", "claude"),
            (CORE + "lib.rs", "after_byte_char_codex", "codex"),
        ],
    },
    {
        "name": "item extent",
        "files": {
            CORE + "lib.rs": """\
struct X { #[cfg(test)] cb: fn(), codex: u8 }
fn f(#[cfg(test)] cb: fn(), claude: u8) {}
struct Y {
    #[cfg(test)]
    fake_field: u8,
}
fn g() { #[cfg(test)] let fake_local = 1; }
#[cfg(test)]
fn generic<A, B>() -> Vec<(A, B)> where A: Into<B> { fake() }
fn after_generic_codex() {}
#[cfg(test)]
fn const_arg() -> Foo<{ 1 }> { fake() }
fn after_const_arg_codex() {}
#[cfg(test)]
const T: Foo = Foo { fake: 1 };
fn after_const_codex() {}
#[cfg(test)]
struct Tuple(u8, Fake);
fn after_tuple_codex() {}
#[cfg(test)]
use fake::{a, b};
fn after_use_codex() {}
#[cfg(test)]
impl<A, B> Fake for (A, B) {}
fn after_impl_codex() {}
#[cfg(test)]
pub(crate) const unsafe fn fake_q() {}
fn after_q_codex() {}
#[cfg(test)]
macro_rules! fake_macro { () => {}; }
fn after_macro_codex() {}
#[cfg(test)]
static mut FAKE: u8 = 0;
fn after_static_codex() {}
fn h() { #[cfg(test)] const { fake() }; }
""",
        },
        "expect": [
            (CORE + "lib.rs", "struct X", "codex"),
            (CORE + "lib.rs", "fn f(", "claude"),
            (CORE + "lib.rs", "fake_field", "fake"),
            (CORE + "lib.rs", "fake_local", "fake"),
            (CORE + "lib.rs", "after_generic_codex", "codex"),
            (CORE + "lib.rs", "after_const_arg_codex", "codex"),
            (CORE + "lib.rs", "after_const_codex", "codex"),
            (CORE + "lib.rs", "after_tuple_codex", "codex"),
            (CORE + "lib.rs", "after_use_codex", "codex"),
            (CORE + "lib.rs", "after_impl_codex", "codex"),
            (CORE + "lib.rs", "after_q_codex", "codex"),
            (CORE + "lib.rs", "after_macro_codex", "codex"),
            (CORE + "lib.rs", "after_static_codex", "codex"),
            (CORE + "lib.rs", "fn h()", "fake"),
        ],
    },
    {
        "name": "attribute and doc clusters",
        "files": {
            CORE + "lib.rs": """\
#[doc = "codex"]
#[cfg(test)]
fn doc_attr() {}
/// fake helper
#[cfg(test)]
fn doc_comment() {}
#[cfg(/* c */ test)]
mod commented { fn fake() {} }
#[cfg(all(test, unix))]
fn composite_fake() {}
/// codex docs
#[derive(Debug)]
struct Production;
#[cfg(test)] // the claude note
fn trailing_comment() {}
fn after_attrs_openai() {}
""",
        },
        "expect": [
            (CORE + "lib.rs", "composite_fake", "fake"),
            (CORE + "lib.rs", "codex docs", "codex"),
            (CORE + "lib.rs", "after_attrs_openai", "openai"),
        ],
    },
    {
        "name": "test-only file modules",
        "files": {
            CORE + "lib.rs": """\
mod engine;
mod nested;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod /* c */ helpers /* d */;
""",
            CORE + "tests.rs": "fn fake() {}\nmod deep;\n",
            CORE + "tests/deep.rs": "fn fake_deep() {}\n",
            CORE + "helpers.rs": "fn fake_helper() {}\n",
            CORE + "engine.rs": "#[cfg(test)]\nmod engine_tests;\n",
            CORE + "engine/engine_tests.rs": "fn codex() {}\n",
            CORE + "nested/mod.rs": "#[cfg(test)]\nmod nested_tests;\n",
            CORE + "nested/nested_tests.rs": "fn claude() {}\n",
        },
        "expect": [
            # A child of a test-only file is scanned: that is conservative.
            (CORE + "tests/deep.rs", "fake_deep", "fake"),
        ],
    },
    {
        "name": "non-test file named tests.rs",
        "files": {
            CORE + "lib.rs": "mod other;\n",
            CORE + "other.rs": "mod tests;\n",
            CORE + "other/tests.rs": "fn anthropic_key() {}\n",
        },
        "expect": [(CORE + "other/tests.rs", "anthropic_key", "anthropic")],
    },
    {
        "name": "nested test declaration",
        "files": {
            CORE + "lib.rs": "mod outer { #[cfg(test)] mod util; }\n",
            CORE + "util.rs": "fn fake_util() {}\n",
            CORE + "outer/util.rs": "fn codex_util() {}\n",
        },
        "expect": [
            (CORE + "util.rs", "fake_util", "fake"),
            (CORE + "outer/util.rs", "codex_util", "codex"),
        ],
    },
    {
        "name": "path alias of a test module",
        "files": {
            CORE + "lib.rs": '#[cfg(test)]\nmod tests;\n#[path = "tests.rs"]\nmod alias;\n',
            CORE + "tests.rs": "fn codex() {}\n",
        },
        "expect": [(CORE + "tests.rs", "codex", "codex")],
    },
    {
        "name": "include of a test module",
        "files": {
            CORE + "lib.rs": '#[cfg(test)]\nmod tests;\ninclude!("tests.rs");\n',
            CORE + "tests.rs": "fn codex() {}\n",
        },
        "expect": [(CORE + "tests.rs", "codex", "codex")],
    },
    {
        "name": "crate root declared test-only",
        "files": {
            CORE + "main.rs": "#[cfg(test)]\nmod lib;\n",
            CORE + "lib.rs": "fn fake_root() {}\n",
        },
        "expect": [(CORE + "lib.rs", "fake_root", "fake")],
    },
    {
        "name": "same module name declared elsewhere in the crate",
        "files": {
            CORE + "lib.rs": "mod engine { mod tests; }\n",
            CORE + "engine.rs": "#[cfg(test)]\nmod tests;\n",
            CORE + "engine/tests.rs": 'const S: &str = "codex";\n',
        },
        "expect": [(CORE + "engine/tests.rs", "const S", "codex")],
    },
    {
        "name": "two test-only declarations of one name",
        "files": {
            CORE + "lib.rs": "mod a;\nmod b;\n",
            CORE + "a.rs": "#[cfg(test)]\nmod tests;\n",
            CORE + "a/tests.rs": "fn fake_a() {}\n",
            CORE + "b.rs": "#[cfg(test)]\nmod tests;\n",
            CORE + "b/tests.rs": "fn fake_b() {}\n",
        },
        "expect": [
            (CORE + "a/tests.rs", "fake_a", "fake"),
            (CORE + "b/tests.rs", "fake_b", "fake"),
        ],
    },
    {
        "name": "computed include",
        "files": {
            CORE + "lib.rs": '#[cfg(test)]\nmod tests;\ninclude!(concat!("tests", ".rs"));\n',
            CORE + "tests.rs": 'const S: &str = "codex";\n',
        },
        "expect": [(CORE + "tests.rs", "const S", "codex")],
    },
    {
        "name": "test item inside macro input",
        "files": {
            CORE + "lib.rs": """\
macro_rules! strip {
    (#[cfg(test)] $item:item) => { $item };
}
strip! {
    #[cfg(test)]
    fn emitted() -> &'static str { "codex" }
}
#[cfg(test)]
fn after_macro() -> &'static str { "fake" }
""",
        },
        "expect": [(CORE + "lib.rs", "fn emitted", "codex")],
    },
    {
        "name": "duplicate declaration",
        "files": {
            CORE + "lib.rs": "#[cfg(test)]\nmod t;\n#[cfg(not(test))]\nmod t;\n",
            CORE + "t.rs": "fn fake_dup() {}\n",
        },
        "expect": [(CORE + "t.rs", "fake_dup", "fake")],
    },
    {
        "name": "names come from the pinned table",
        "table": SENTINEL_TABLE,
        "files": {
            CORE + "lib.rs": "fn zeta_mode() {}\nfn omega_mode() {}\nfn wrong_mode() {}\nfn codex() {}\n",
        },
        "expect": [
            (CORE + "lib.rs", "zeta_mode", "zeta"),
            (CORE + "lib.rs", "omega_mode", "omega"),
        ],
    },
    {
        "name": "allow entries cover only their substring",
        "allow": (
            "# path: substring: reason\n"
            + CORE + 'lib.rs: "claude": self-test entry\n'
            + CORE + "lib.rs: claude: identifier is wider than the entry\n"
            + CLI + 'main.rs: "codex": another file\n'
        ),
        "files": {
            CORE + "lib.rs": 'const CODEX_X: &str = "claude";\nfn claude_fn() {}\nconst A: &str = "codex";\n',
            CLI + "main.rs": 'const B: &str = "codex";\n',
        },
        "expect": [
            (CORE + "lib.rs", "CODEX_X", "codex"),
            (CORE + "lib.rs", "claude_fn", "claude"),
            (CORE + "lib.rs", "const A", "codex"),
        ],
    },
]

ERROR_CASES = [
    ("missing harness.rs", None, None),
    ("no table", "pub const X: u8 = 0;\n", None),
    ("private table", 'const HARNESSES: &[HarnessRow] = &[HarnessRow { name: "a" }];\n', None),
    ("empty table", "pub const HARNESSES: &[HarnessRow] = &[];\n", None),
    (
        "row without a literal name",
        'pub const HARNESSES: &[HarnessRow] = &[HarnessRow { name: "a" }, HarnessRow { name: NAME }];\n',
        None,
    ),
    (
        "two tables",
        'pub const HARNESSES: &[HarnessRow] = &[HarnessRow { name: "a" }];\n' * 2,
        None,
    ),
    ("raw name", 'pub const HARNESSES: &[HarnessRow] = &[HarnessRow { name: r"a" }];\n', None),
    ("malformed allow entry", TABLE, "crates/via-core/src/lib.rs: no reason\n"),
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


def expected_findings(case):
    """`path:line: name` for each expectation, located by its marker."""
    expected = set()
    for path, marker, name in case["expect"]:
        lines = [n for n, text in enumerate(case["files"][path].split("\n"), 1) if marker in text]
        if len(lines) != 1:
            raise AssertionError(f"marker {marker!r} is not on exactly one line of {path}")
        expected.add(f"{path}:{lines[0]}: {name}")
    return expected


def self_test():
    failures = []
    with tempfile.TemporaryDirectory() as tmp:
        for number, case in enumerate(CASES):
            root = Path(tmp) / f"case-{number}"
            write_tree(root, case["files"], case.get("table", TABLE), case.get("allow"))
            try:
                found = {": ".join(f.split(": ")[:2]) for f in check(root)}
            except GuardError as error:
                failures.append(f"{case['name']}: unexpected error: {error}")
                continue
            expected = expected_findings(case)
            if found != expected:
                failures.append(
                    f"{case['name']}:\n  missing: {sorted(expected - found)}"
                    f"\n  unexpected: {sorted(found - expected)}"
                )
        for number, (name, table, allow) in enumerate(ERROR_CASES):
            root = Path(tmp) / f"error-{number}"
            write_tree(root, {CORE + "lib.rs": "fn main() {}\n"}, table, allow)
            try:
                check(root)
                failures.append(f"{name}: no error")
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
