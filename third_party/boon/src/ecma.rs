use std::borrow::Cow;

use regex_syntax::ast::parse::Parser;
use regex_syntax::ast::{self, *};

// covert ecma regex to rust regex if possible
// see https://262.ecma-international.org/11.0/#sec-regexp-regular-expression-objects
//
// VIA patch: linear in the pattern's length. Upstream fixed one `\c` escape
// or translated one perl class per round, reparsing the whole pattern each
// time (quadratic). The result is upstream's but for the ECMA fixes below
// (critical r2 #7): every `\c{letter}` the parser reaches as an escape
// becomes its control character, then every perl class, `.`, `\b` and `\B`
// is replaced in one pass over one parse.
pub(crate) fn convert(pattern: &str) -> Result<Cow<str>, Box<dyn std::error::Error>> {
    convert_with(pattern, &ECMA)
}

// VIA patch (critical r3 #3): the conversion with `with`'s replacement
// texts for the ECMA fixes; the tests' oracle passes upstream's, so only
// replacements of translated nodes ever differ, never the caller's text.
fn convert_with<'p>(
    pattern: &'p str,
    with: &'static Replacements,
) -> Result<Cow<'p, str>, Box<dyn std::error::Error>> {
    let (pattern, ast) = parse_fixing_controls(pattern)?;
    let mut spans = ast::visit(
        &ast,
        Translator {
            spans: Vec::new(),
            with,
        },
    )?;
    if spans.is_empty() {
        return Ok(pattern);
    }
    spans.sort_unstable_by_key(|(start, _, _)| *start);
    spans.dedup_by_key(|(start, _, _)| *start);
    let mut out = String::with_capacity(pattern.len() + 8 * spans.len());
    let mut at = 0;
    for (start, end, with) in spans {
        out.push_str(&pattern[at..start]);
        out.push_str(with);
        at = end;
    }
    out.push_str(&pattern[at..]);
    Ok(Cow::Owned(out))
}

// VIA patch (critical r2 #7): ECMA-262's meaning of `\s`, `.` and `\b`,
// where upstream kept Rust's. `\s` is WhiteSpace plus LineTerminator
// (upstream's set lacked U+1680, U+2000-U+2002, U+2004-U+200A, U+2028,
// U+202F, U+205F and U+3000); `.` is any character but a LineTerminator
// (Rust's excludes only U+000A); `\b` and `\B` take ECMA's word
// characters `[A-Za-z0-9_]` (Rust's are Unicode's). JSON Schema patterns
// carry no flags. Every character is written as an escape, so an
// extended-mode pattern keeps it.
const SPACE: &str = r"[\t\n\x0B\x0C\r\x20\u{A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]";
const NOT_SPACE: &str = r"[^\t\n\x0B\x0C\r\x20\u{A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]";
const DOT: &str = r"[^\n\r\u{2028}\u{2029}]";
const WORD_BOUNDARY: &str = r"(?-u:\b)";
const NOT_WORD_BOUNDARY: &str = r"(?-u:\B)";

// VIA patch (critical r3 #3): the replacement texts of the ECMA fixes.
struct Replacements {
    space: &'static str,
    not_space: &'static str,
    dot: &'static str,
    word_boundary: &'static str,
    not_word_boundary: &'static str,
}

const ECMA: Replacements = Replacements {
    space: SPACE,
    not_space: NOT_SPACE,
    dot: DOT,
    word_boundary: WORD_BOUNDARY,
    not_word_boundary: NOT_WORD_BOUNDARY,
};

// VIA patch: most `\c` fixes an extended-mode pattern may need; past it the
// pattern is refused rather than reparsed again.
const EXTENDED_CONTROL_FIXES: usize = 32;

// VIA patch: the pattern with every `\c{letter}` escape replaced by its
// control character, as upstream's fix-and-reparse loop leaves it, and its
// parse. Without extended mode no comment can hide a `\c`, so one scan
// fixes every escape the parser would reach, and one parse follows; with
// it, upstream's loop runs, fixing at most [`EXTENDED_CONTROL_FIXES`]
// escapes and parsing after each.
fn parse_fixing_controls(pattern: &str) -> Result<(Cow<str>, Ast), Box<dyn std::error::Error>> {
    let first = match Parser::new().parse(pattern) {
        Ok(ast) => return Ok((Cow::Borrowed(pattern), ast)),
        Err(e) => e,
    };
    let Some(mut fixed) = fix_error(&first) else {
        Err(first)?
    };
    if !(pattern.contains("(?") && pattern.contains('#')) {
        let fixed = fix_controls(pattern);
        let ast = Parser::new().parse(&fixed)?;
        return Ok((Cow::Owned(fixed), ast));
    }
    // VIA patch (critical r2 #9): the pattern is parsed after each fix,
    // the last allowed one included.
    let mut fixes = 1;
    loop {
        match Parser::new().parse(&fixed) {
            Ok(ast) => return Ok((Cow::Owned(fixed), ast)),
            Err(e) => match fix_error(&e) {
                Some(_) if fixes == EXTENDED_CONTROL_FIXES => {
                    Err("too many control escapes in an extended-mode pattern")?
                }
                Some(s) => {
                    fixed = s;
                    fixes += 1;
                }
                None => Err(e)?,
            },
        }
    }
}

// VIA patch: every `\c{ascii letter}` at an escape position (each `\`
// takes the character after it) replaced by its control character.
fn fix_controls(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('c') => match chars.peek() {
                Some(&letter) if letter.is_ascii_alphabetic() => {
                    chars.next();
                    out.push(((letter as u8) % 32) as char);
                }
                _ => out.push_str(r"\c"),
            },
            Some(next) => {
                out.push('\\');
                out.push(next);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn fix_error(e: &Error) -> Option<String> {
    if let ErrorKind::EscapeUnrecognized = e.kind() {
        let (start, end) = (e.span().start.offset, e.span().end.offset);
        let s = &e.pattern()[start..end];
        if let r"\c" = s {
            // handle \c{control_letter}
            if let Some(control_letter) = e.pattern()[end..].chars().next() {
                if control_letter.is_ascii_alphabetic() {
                    return Some(format!(
                        "{}{}{}",
                        &e.pattern()[..start],
                        ((control_letter as u8) % 32) as char,
                        &e.pattern()[end + 1..],
                    ));
                }
            }
        }
    }
    None
}

/**
handles following translations:
-  \d should ascii digits only. so replace with [0-9]
-  \D should match everything but ascii digits. so replace with [^0-9]
-  \w should match ascii letters only. so replace with [a-zA-Z0-9_]
-  \W should match everything but ascii letters. so replace with [^a-zA-Z0-9_]
-  \s and \S differences
-  \a is not an ECMA 262 control escape

VIA patch: collects every replacement of one parse, as byte spans.
*/
struct Translator {
    spans: Vec<(usize, usize, &'static str)>,
    with: &'static Replacements,
}

impl Translator {
    fn replace_class_class(&mut self, perl: &ClassPerl) {
        let with = match perl.kind {
            ClassPerlKind::Digit => {
                if perl.negated {
                    "[^0-9]"
                } else {
                    "[0-9]"
                }
            }
            ClassPerlKind::Word => {
                if perl.negated {
                    "[^A-Za-z0-9_]"
                } else {
                    "[A-Za-z0-9_]"
                }
            }
            ClassPerlKind::Space => {
                if perl.negated {
                    self.with.not_space
                } else {
                    self.with.space
                }
            }
        };
        self.replace(&perl.span, with);
    }

    fn replace(&mut self, span: &Span, with: &'static str) {
        self.spans.push((span.start.offset, span.end.offset, with));
    }
}

impl Visitor for Translator {
    type Output = Vec<(usize, usize, &'static str)>;
    type Err = &'static str;

    fn finish(self) -> Result<Self::Output, Self::Err> {
        Ok(self.spans)
    }

    fn visit_class_set_item_pre(&mut self, ast: &ast::ClassSetItem) -> Result<(), Self::Err> {
        if let ClassSetItem::Perl(perl) = ast {
            self.replace_class_class(perl);
        }
        Ok(())
    }

    fn visit_post(&mut self, ast: &Ast) -> Result<(), Self::Err> {
        match ast {
            Ast::ClassPerl(perl) => {
                self.replace_class_class(perl);
            }
            // VIA patch (critical r2 #7).
            Ast::Dot(span) => self.replace(span, self.with.dot),
            Ast::Assertion(assertion) => match assertion.kind {
                AssertionKind::WordBoundary => {
                    self.replace(&assertion.span, self.with.word_boundary);
                }
                AssertionKind::NotWordBoundary => {
                    self.replace(&assertion.span, self.with.not_word_boundary);
                }
                _ => (),
            },
            Ast::Literal(ref literal) => {
                if let Literal {
                    kind: LiteralKind::Special(SpecialLiteralKind::Bell),
                    ..
                } = literal.as_ref()
                {
                    return Err("\\a is not an ECMA 262 control escape");
                }
            }
            _ => (),
        }
        Ok(())
    }
}

// VIA patch: upstream 0.6.1's conversion, verbatim, the oracle the linear
// conversion is tested against.
#[cfg(test)]
mod upstream {
    use std::borrow::Cow;

    use regex_syntax::ast::parse::Parser;
    use regex_syntax::ast::{self, *};

    // covert ecma regex to rust regex if possible
    // see https://262.ecma-international.org/11.0/#sec-regexp-regular-expression-objects
    pub(super) fn convert(pattern: &str) -> Result<Cow<str>, Box<dyn std::error::Error>> {
        let mut pattern = Cow::Borrowed(pattern);

        let mut ast = loop {
            match Parser::new().parse(pattern.as_ref()) {
                Ok(ast) => break ast,
                Err(e) => {
                    if let Some(s) = fix_error(&e) {
                        pattern = Cow::Owned(s);
                    } else {
                        Err(e)?;
                    }
                }
            }
        };

        loop {
            let translator = Translator {
                pat: pattern.as_ref(),
                out: None,
            };
            if let Some(updated_pattern) = ast::visit(&ast, translator)? {
                match Parser::new().parse(&updated_pattern) {
                    Ok(updated_ast) => {
                        pattern = Cow::Owned(updated_pattern);
                        ast = updated_ast;
                    }
                    Err(e) => {
                        debug_assert!(
                            false,
                            "ecma::translate changed {:?} to {:?}: {e}",
                            pattern, updated_pattern
                        );
                        break;
                    }
                }
            } else {
                break;
            }
        }
        Ok(pattern)
    }

    fn fix_error(e: &Error) -> Option<String> {
        if let ErrorKind::EscapeUnrecognized = e.kind() {
            let (start, end) = (e.span().start.offset, e.span().end.offset);
            let s = &e.pattern()[start..end];
            if let r"\c" = s {
                // handle \c{control_letter}
                if let Some(control_letter) = e.pattern()[end..].chars().next() {
                    if control_letter.is_ascii_alphabetic() {
                        return Some(format!(
                            "{}{}{}",
                            &e.pattern()[..start],
                            ((control_letter as u8) % 32) as char,
                            &e.pattern()[end + 1..],
                        ));
                    }
                }
            }
        }
        None
    }

    /**
    handles following translations:
    -  \d should ascii digits only. so replace with [0-9]
    -  \D should match everything but ascii digits. so replace with [^0-9]
    -  \w should match ascii letters only. so replace with [a-zA-Z0-9_]
    -  \W should match everything but ascii letters. so replace with [^a-zA-Z0-9_]
    -  \s and \S differences
    -  \a is not an ECMA 262 control escape
    */
    struct Translator<'a> {
        pat: &'a str,
        out: Option<String>,
    }

    impl Translator<'_> {
        fn replace(&mut self, span: &Span, with: &str) {
            let (start, end) = (span.start.offset, span.end.offset);
            self.out = Some(format!("{}{with}{}", &self.pat[..start], &self.pat[end..]));
        }

        fn replace_class_class(&mut self, perl: &ClassPerl) {
            match perl.kind {
                ClassPerlKind::Digit => {
                    self.replace(&perl.span, if perl.negated { "[^0-9]" } else { "[0-9]" });
                }
                ClassPerlKind::Word => {
                    let with = &if perl.negated {
                        "[^A-Za-z0-9_]"
                    } else {
                        "[A-Za-z0-9_]"
                    };
                    self.replace(&perl.span, with);
                }
                ClassPerlKind::Space => {
                    let with = &if perl.negated {
                        "[^ \t\n\r\u{000b}\u{000c}\u{00a0}\u{feff}\u{2003}\u{2029}]"
                    } else {
                        "[ \t\n\r\u{000b}\u{000c}\u{00a0}\u{feff}\u{2003}\u{2029}]"
                    };
                    self.replace(&perl.span, with);
                }
            }
        }
    }

    impl Visitor for Translator<'_> {
        type Output = Option<String>;
        type Err = &'static str;

        fn finish(self) -> Result<Self::Output, Self::Err> {
            Ok(self.out)
        }

        fn visit_class_set_item_pre(&mut self, ast: &ast::ClassSetItem) -> Result<(), Self::Err> {
            if let ClassSetItem::Perl(perl) = ast {
                self.replace_class_class(perl);
            }
            Ok(())
        }

        fn visit_post(&mut self, ast: &Ast) -> Result<(), Self::Err> {
            if self.out.is_some() {
                return Ok(());
            }
            match ast {
                Ast::ClassPerl(perl) => {
                    self.replace_class_class(perl);
                }
                Ast::Literal(ref literal) => {
                    if let Literal {
                        kind: LiteralKind::Special(SpecialLiteralKind::Bell),
                        ..
                    } = literal.as_ref()
                    {
                        return Err("\\a is not an ECMA 262 control escape");
                    }
                }
                _ => (),
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ecma_compat_valid() {
        // println!("{:#?}", Parser::new().parse(r#"a\a"#));
        let tests = [
            (r"ab\cAcde\cBfg", "ab\u{1}cde\u{2}fg"), // \c{control_letter}
            (r"\\comment", r"\\comment"),            // there is no \c
            (r"ab\def", r#"ab[0-9]ef"#),             // \d
            (r"ab[a-z\d]ef", r#"ab[a-z[0-9]]ef"#),   // \d inside classSet
            (r"ab\Def", r#"ab[^0-9]ef"#),            // \d
            (r"ab[a-z\D]ef", r#"ab[a-z[^0-9]]ef"#),  // \D inside classSet
        ];
        for (input, want) in tests {
            match convert(input) {
                Ok(got) => {
                    if got.as_ref() != want {
                        panic!("convert({input:?}): got: {got:?}, want: {want:?}");
                    }
                }
                Err(e) => {
                    panic!("convert({input:?}) failed: {e}");
                }
            }
        }
    }

    #[test]
    fn test_ecma_compat_invalid() {
        // println!("{:#?}", Parser::new().parse(r#"a\a"#));
        let tests = [
            r"\c\n",     // \c{invalid_char}
            r"abc\adef", // \a is not valid
        ];
        for input in tests {
            if convert(input).is_ok() {
                panic!("convert({input:?}) mut fail");
            }
        }
    }

    // VIA patch: the patterns of the JSON-Schema-Test-Suite's draft2020-12
    // `pattern`, `patternProperties` and `optional/ecmascript-regex` and
    // `optional/format/regex` files (written out; the suite is not vendored),
    // and the escapes the conversion rewrites.
    const SUITE: &[&str] = &[
        "^a*$",
        "^a+$",
        "a+",
        "f.*o",
        "a*",
        "aaa*",
        "[0-9]{2,}",
        "X_",
        "^.*c",
        "^.*b",
        "^[a-z]*$",
        "^abc$",
        r"^\cC$",
        r"^\cc$",
        r"^\d$",
        r"^\D$",
        r"^\w$",
        r"^\W$",
        r"^\s$",
        r"^\S$",
        r"\a",
        r"^\p{Letter}cole",
        r"\wcole",
        "[a-z]cole",
        r"^\d+$",
        r"^\p{digit}+$",
        r"\p{Letter}cole",
        r"^\p{L}+$",
        "^(abc]",
        r"[\d]",
        r"[^\d]",
        r"[\w-]",
        r"[\s\S]",
        r"\\d",
        r"\\\d",
        r"\cA\cB\c1",
        r"[\cA-\cZ]",
        r"(?x)a # \cA",
        r"(?x)\cJ*",
        r"(?x)a#\cJb",
        r"(?i)\w+",
        r"[[:alpha:]\d]",
        r"[a&&\d]",
        r"^[\p{L}\p{N}]+$",
        "",
        r"\",
        r"\c",
        r"a\cIb",
        r"(?P<\cA>x)",
        r"a{\cA}",
    ];

    // VIA patch (critical r2 #7, r3 #3): upstream's text for each ECMA
    // fix's replacement, so the differential checks that the conversion
    // differs from upstream's only in the replacements of the nodes those
    // fixes translate.
    const UPSTREAM: Replacements = Replacements {
        space: "[ \t\n\r\u{000b}\u{000c}\u{00a0}\u{feff}\u{2003}\u{2029}]",
        not_space: "[^ \t\n\r\u{000b}\u{000c}\u{00a0}\u{feff}\u{2003}\u{2029}]",
        dot: ".",
        word_boundary: r"\b",
        not_word_boundary: r"\B",
    };

    fn same(input: &str) {
        let got = convert_with(input, &UPSTREAM)
            .map(|c| c.into_owned())
            .map_err(|_| ());
        let want = super::upstream::convert(input)
            .map(|c| c.into_owned())
            .map_err(|_| ());
        assert_eq!(got, want, "convert({input:?})");
    }

    // VIA patch (critical r3 #5): `[\b]`, ECMA's backspace in a bracket
    // class, is refused, as upstream's conversion refuses it.
    #[test]
    fn a_class_backspace_is_refused() {
        assert!(convert(r"[\b]").is_err());
        assert!(super::upstream::convert(r"[\b]").is_err());
    }

    // VIA patch (critical r3 #3): patterns whose own text is a
    // replacement's form; neither conversion rewrites them.
    #[test]
    fn matches_upstream_on_caller_authored_replacement_forms() {
        for input in [
            DOT,
            SPACE,
            NOT_SPACE,
            WORD_BOUNDARY,
            NOT_WORD_BOUNDARY,
            r"a[^\n\r\u{2028}\u{2029}]b",
            r"(?x)[^\n\r\u{2028}\u{2029}]#\cA",
        ] {
            same(input);
        }
    }

    #[test]
    fn matches_upstream_on_the_suite() {
        for input in SUITE {
            same(input);
        }
    }

    // VIA patch: seeded random patterns over the tokens the conversion
    // treats specially, extended-mode comments included.
    #[test]
    fn matches_upstream_on_random_patterns() {
        const TOKENS: &[&str] = &[
            r"\d", r"\D", r"\w", r"\W", r"\s", r"\S", r"\cA", r"\cz", r"\cJ", r"\c1", r"\c", r"\\",
            r"\a", "[", "]", "[^", "-", "(", ")", "(?x)", "(?-x)", "(?x:", "(?i)", "#", "\n", " ",
            "a", "b", "*", "+", "?", "{2}", "|", "^", "$", ".", r"\p{L}", r"\x41", "&&", r"\b",
            r"\B", r"\.",
        ];
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as usize
        };
        for _ in 0..100_000 {
            let len = next() % 12;
            let input: String = (0..len).map(|_| TOKENS[next() % TOKENS.len()]).collect();
            same(&input);
        }
    }

    // VIA patch (critical r1 #1): a pattern of repeated `\d` converts in
    // time linear in its length; upstream took about a second at 6 KiB.
    #[test]
    fn repeated_escapes_convert_in_linear_time() {
        let input = r"\d".repeat(12 * 1024);
        let start = std::time::Instant::now();
        let got = convert(&input).expect("converts");
        assert_eq!(got.as_ref(), "[0-9]".repeat(12 * 1024));
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "took {elapsed:?}"
        );
    }

    // VIA patch (critical r2 #9): an extended-mode pattern needing 32
    // control-escape fixes converts; one needing 33 is refused.
    #[test]
    fn extended_mode_takes_exactly_32_control_fixes() {
        for (fixes, converts) in [(31, true), (32, true), (33, false)] {
            let input = format!("(?x)#\n{}", r"\cA".repeat(fixes));
            let got = convert(&input).map(|c| c.into_owned());
            assert_eq!(got.is_ok(), converts, "{fixes} fixes: {got:?}");
            if let Ok(got) = got {
                assert_eq!(got, format!("(?x)#\n{}", "\u{1}".repeat(fixes)));
            }
        }
    }

    fn matches(pattern: &str, subject: &str) -> bool {
        let converted = convert(pattern).expect("converts");
        regex::Regex::new(&converted)
            .expect("compiles")
            .is_match(subject)
    }

    // VIA patch (critical r2 #7): `\s` and `\S` are ECMA-262's WhiteSpace
    // and LineTerminator set and its complement, inside bracket classes
    // too; U+0085, in Rust's `\s`, is not in it.
    #[test]
    fn space_classes_are_ecma_whitespace() {
        let spaces = [
            '\t', '\n', '\u{b}', '\u{c}', '\r', ' ', '\u{a0}', '\u{1680}', '\u{2000}', '\u{2009}',
            '\u{200a}', '\u{2028}', '\u{2029}', '\u{202f}', '\u{205f}', '\u{3000}', '\u{feff}',
        ];
        let others = ['a', '0', '_', '\u{85}', '\u{180e}', '\u{200b}', '\u{1}'];
        for (subject, space) in spaces
            .iter()
            .map(|c| (c, true))
            .chain(others.iter().map(|c| (c, false)))
        {
            let subject = subject.to_string();
            for (pattern, want) in [
                (r"^\s$", space),
                (r"^\S$", !space),
                (r"^[\s]$", space),
                (r"^[\S]$", !space),
                (r"^[^\s]$", !space),
                (r"^[^\S]$", space),
                (r"^[x\s]$", space),
            ] {
                assert_eq!(matches(pattern, &subject), want, "{pattern} on {subject:?}");
            }
        }
    }

    // VIA patch (critical r2 #7): `.` is any character but the four line
    // terminators; inside a bracket class it is a literal dot.
    #[test]
    fn dot_excludes_ecma_line_terminators() {
        for (subject, dot) in [
            ("\n", false),
            ("\r", false),
            ("\u{2028}", false),
            ("\u{2029}", false),
            ("a", true),
            ("\u{85}", true),
            ("\t", true),
            ("\u{1f600}", true),
        ] {
            assert_eq!(matches("^.$", subject), dot, "dot on {subject:?}");
            assert_eq!(matches("^[.]$", subject), false, "[.] on {subject:?}");
        }
        assert!(matches("^a.c$", "abc"));
        assert!(!matches("^a.c$", "a\r\nc"));
        assert!(matches("^[.]$", "."));
        assert!(matches(r"^\.$", "."));
        assert!(!matches(r"^\.$", "a"));
    }

    // VIA patch (critical r2 #7): `\b` and `\B` use ECMA-262's word
    // characters, `[A-Za-z0-9_]`.
    #[test]
    fn word_boundaries_are_ascii() {
        for (pattern, subject, want) in [
            (r"\bfoo\b", "αfooβ", true),
            (r"\bfoo\b", "afoob", false),
            (r"\bβ\b", " β ", false),
            (r"^a\Bβ$", "aβ", false),
            (r"^a\Bb$", "ab", true),
            (r"^β\Bγ$", "βγ", true),
            (r"\Bfoo", "αfoo", false),
            (r"\Bfoo", "xfoo", true),
        ] {
            assert_eq!(matches(pattern, subject), want, "{pattern} on {subject:?}");
        }
    }

    // VIA patch (critical r2 #7): the classes upstream already translated
    // keep ECMA-262's ASCII meaning, inside bracket classes too.
    #[test]
    fn digit_and_word_classes_are_ascii() {
        for (pattern, subject, want) in [
            (r"^\d$", "5", true),
            (r"^\d$", "\u{663}", false),
            (r"^\D$", "\u{663}", true),
            (r"^[\d]$", "5", true),
            (r"^[\d]$", "\u{663}", false),
            (r"^[\D]$", "\u{663}", true),
            (r"^\w$", "_", true),
            (r"^\w$", "é", false),
            (r"^\W$", "é", true),
            (r"^[\w]$", "z", true),
            (r"^[\w]$", "é", false),
            (r"^[\W]$", "é", true),
        ] {
            assert_eq!(matches(pattern, subject), want, "{pattern} on {subject:?}");
        }
    }
}
