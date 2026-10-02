//! C1 Q2 structured-output validation (adapter design §5.1 #37, K14): a
//! frozen `output_schema` compiled as JSON Schema draft 2020-12 with the
//! vendored boon (`third_party/boon/VIA-PATCH.md`). The compiler is given
//! no URL loader, so a schema that refers to anything outside itself, a
//! file or a network address, does not compile; one that declares another
//! draft anywhere does not compile either. Compiling and validating are
//! bounded (fix round 1 #1), its metaschema check included (fix round 2
//! #3), and both run off the async executor.
//!
//! The bounds were sized by measurement (release build). These are the
//! results for the recorded cases, not a general guarantee: validating a
//! value at C1's maximum (65,536 nodes, depth 64) against a moderately
//! complex schema needed 381,056 units in about 3 ms; the slowest
//! adversarial validation spent the whole budget in about 55 ms, and the
//! largest peak was 36,864 KiB (27,860 KiB in fix round 1, before the
//! cases added in round 2); the compiles measured stayed under about
//! 70 ms; a 256 KiB schema of about 2,000 subschemas needed about 325,000
//! metaschema units. Patterns (critical fix round 1): a 24 KiB pattern of
//! `\d` compiled in about 12 ms (over 8 s before), and the pattern sets
//! measured at the 256 KiB cap were refused within about 65 ms.

use boon::{Compiler, Draft, SchemaIndex, Schemas, SchemeUrlLoader, Validation};
use serde_json::Value;

/// The location the one schema is compiled under; nothing is loaded from it.
const LOCATION: &str = "urn:via:output_schema";

/// Most subschemas one `output_schema` has, counted over every schema
/// position, reached or not (critical r1 #9).
const SUBSCHEMAS: usize = 2048;

/// Most `pattern` and `patternProperties` expressions in one schema,
/// counted the same way; each must build within the regex program limit.
const PATTERNS: usize = 64;

/// Work units one validation may spend (boon patch: one per subschema
/// evaluation, more for keywords whose work grows with the value).
const UNITS: u64 = 1_000_000;

/// Work units every metaschema check of one compile may spend together
/// (boon patch): the schema's own, and those of the values its references
/// reach outside the known subschemas.
const META_UNITS: u64 = 1_000_000;

/// Deepest evaluation nesting one validation may reach.
const DEPTH: usize = 512;

/// Stack of the thread that runs a compile or a validation: room for
/// [`DEPTH`] active evaluations, nested validations' included, in an
/// unoptimized build (about 500 needed 4 MiB there, and 512 KiB in a
/// release build).
const STACK: usize = 16 << 20;

/// A validation's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Checked {
    /// The value satisfies the schema.
    Valid,
    /// The value does not satisfy the schema.
    Invalid,
    /// The work or depth bound was reached first (`validation_limit`).
    Limit,
}

/// A compiled `output_schema`.
pub(crate) struct Validator {
    schemas: Schemas,
    index: SchemaIndex,
}

impl Validator {
    /// `schema` compiled as draft 2020-12 (C1 §4) within the compile
    /// limits, or `None` when it is not a valid draft 2020-12 schema,
    /// refers outside itself or exceeds a limit. Runs on the caller's
    /// thread: use [`compiles`] or [`validate`] from async code.
    pub(crate) fn compile(schema: &Value) -> Option<Self> {
        let mut compiler = Compiler::new();
        compiler.set_default_draft(Draft::V2020_12);
        compiler.require_draft(Draft::V2020_12);
        compiler.set_limits(SUBSCHEMAS, PATTERNS);
        compiler.set_metaschema_budget(META_UNITS, DEPTH);
        compiler.use_loader(Box::new(SchemeUrlLoader::new()));
        compiler.add_resource(LOCATION, schema.clone()).ok()?;
        let mut schemas = Schemas::new();
        let index = compiler.compile(LOCATION, &mut schemas).ok()?;
        Some(Self { schemas, index })
    }

    /// Whether `value` satisfies the schema, within the bounds. Recurses
    /// up to [`DEPTH`] evaluations: run it on a thread with [`STACK`].
    pub(crate) fn check(&self, value: &Value) -> Checked {
        match self
            .schemas
            .validate_within(value, self.index, UNITS, DEPTH)
        {
            Validation::Valid => Checked::Valid,
            Validation::Invalid => Checked::Invalid,
            Validation::BudgetSpent => Checked::Limit,
        }
    }
}

/// Runs `work` on a scoped thread with [`STACK`] and waits for it: room
/// for [`DEPTH`] nested evaluations, and for compiling's metaschema check,
/// in any build. Callers run it as a Store blocking step, off the async
/// executor and owned until it ends (coding-style §5); the compile limits
/// and the validation budget bound its work. `None` when no thread could
/// be started.
fn on_stack<T: Send>(work: impl FnOnce() -> T + Send) -> Option<T> {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("via-schema".to_owned())
            .stack_size(STACK)
            .spawn_scoped(scope, work)
            .ok()?
            .join()
            .ok()
    })
}

/// Whether `schema` compiles (intake, C1 §4). Blocking: see [`on_stack`].
pub(crate) fn compiles(schema: &Value) -> bool {
    on_stack(|| Validator::compile(schema).is_some()).unwrap_or(false)
}

/// `value` checked against `schema`. Blocking: see [`on_stack`]. A schema
/// that no longer compiles, or a check that could not run, is `Limit`:
/// intake admitted the schema, so it compiled then.
pub(crate) fn validate(schema: &Value, value: &Value) -> Checked {
    on_stack(|| {
        Validator::compile(schema).map_or(Checked::Limit, |validator| validator.check(value))
    })
    .unwrap_or(Checked::Limit)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use serde_json::Value;

    use super::{Checked, Validator, compiles, validate};

    /// Draft 2020-12 rules apply; an invalid schema and one that refers
    /// to a file or the network do not compile.
    #[test]
    fn schemas_compile_only_self_contained_and_valid() {
        let schema = json!({"type":"object","properties":{"a":{"type":"integer"}},
                            "required":["a"],"dependentRequired":{"a":[]}});
        let validator = Validator::compile(&schema).unwrap();
        assert_eq!(validator.check(&json!({"a":1})), Checked::Valid);
        assert_eq!(validator.check(&json!({"a":"1"})), Checked::Invalid);
        assert_eq!(validator.check(&json!({"b":1})), Checked::Invalid);
        assert!(Validator::compile(&json!({"type":5})).is_none());
        for outside in ["file:///etc/passwd", "https://example.com/schema.json"] {
            assert!(
                Validator::compile(&json!({"$ref": outside})).is_none(),
                "{outside}"
            );
        }
    }

    /// Fix round 1 #2: a `$schema` naming another draft, at the root or in
    /// an embedded resource, does not compile (Sol's draft-07
    /// `dependentRequired` case: draft-07 ignores the keyword).
    #[test]
    fn only_draft_2020_12_compiles() {
        let draft07 = json!({"$schema":"http://json-schema.org/draft-07/schema#",
                             "dependentRequired":{"a":["b"]}});
        assert!(Validator::compile(&draft07).is_none());
        let embedded = json!({"$defs":{"x":{"$id":"urn:via:x",
                              "$schema":"http://json-schema.org/draft-07/schema#"}},
                              "dependentRequired":{"a":["b"]}});
        assert!(Validator::compile(&embedded).is_none());
        let declared = json!({"$schema":"https://json-schema.org/draft/2020-12/schema",
                              "dependentRequired":{"a":["b"]}});
        let validator = Validator::compile(&declared).unwrap();
        assert_eq!(validator.check(&json!({"a":1})), Checked::Invalid);
    }

    /// Fix round 2, C1 §4: a `$schema` inside an instance value (`const`,
    /// `enum`, `default`, `examples`) is data, not a declaration: the
    /// schema compiles, and the value is matched as written.
    #[test]
    fn schema_keywords_inside_instance_values_are_data() {
        let other = json!({"$schema":"http://json-schema.org/draft-07/schema#"});
        let schema = json!({
            "properties":{
                "c":{"const": other},
                "e":{"enum":[other, 1]},
                "d":{"default": other, "examples":[other]}
            }
        });
        let validator = Validator::compile(&schema).unwrap();
        assert_eq!(
            validator.check(&json!({"c": other, "e": other, "d": 1})),
            Checked::Valid
        );
        assert_eq!(validator.check(&json!({"c": {}})), Checked::Invalid);
    }

    /// Fix round 1 #1: validation work is bounded. Sol's reference-doubling
    /// `$defs` chain (exponential work, and an error tree that exhausted
    /// 256 MiB at 1,562 bytes) ends at the budget as `Limit`.
    #[test]
    fn doubling_references_end_at_the_limit() {
        let mut defs = serde_json::Map::new();
        defs.insert("d0".into(), json!({"type":"string"}));
        for i in 1..=40 {
            let previous = json!({"$ref": format!("#/$defs/d{}", i - 1)});
            defs.insert(
                format!("d{i}"),
                json!({"anyOf":[previous.clone(), previous]}),
            );
        }
        let schema = json!({"$ref":"#/$defs/d40","$defs":defs});
        assert_eq!(validate(&schema, &json!(1)), Checked::Limit);
        assert_eq!(validate(&schema, &json!("s")), Checked::Valid);
    }

    /// `{"a":{"a":…1}}`, `depth` levels deep.
    fn nested(depth: usize) -> Value {
        (0..depth).fold(json!(1), |inner, _| json!({ "a": inner }))
    }

    /// Fix round 1 #1, the ruling's adversarial cases: `allOf`/`anyOf`
    /// fanning out on the same key across depth 64; `uniqueItems` and a
    /// large `enum` over maximal arrays; long strings against patterns.
    /// Each ends within the bounds, at the limit or with its answer.
    #[test]
    fn adversarial_values_end_within_the_bounds() {
        // `allOf` over a valid leaf, `anyOf` over a failing one: every
        // branch is evaluated at every level.
        for (keyword, leaf) in [("allOf", "integer"), ("anyOf", "object")] {
            let branch = json!({"properties":{"a":{"$ref":"#/$defs/n"}},
                                "if":{"type":"integer"},"then":{"type":leaf}});
            let schema =
                json!({"$ref":"#/$defs/n","$defs":{"n":{ keyword: [branch.clone(), branch] }}});
            assert_eq!(validate(&schema, &nested(63)), Checked::Limit, "{keyword}");
        }
        let distinct: Vec<Value> = (0..65_535).map(|i| json!(i)).collect();
        assert_eq!(
            validate(
                &json!({"uniqueItems":true}),
                &Value::Array(distinct.clone())
            ),
            Checked::Valid
        );
        let all: Vec<Value> = (0..1000).map(|_| json!({"uniqueItems":true})).collect();
        assert_eq!(
            validate(&json!({ "allOf": all }), &Value::Array(distinct)),
            Checked::Limit
        );
        let listed: Vec<Value> = (0..20_000).map(|i| json!(format!("e{i:06}"))).collect();
        let items: Vec<Value> = (0..65_535).map(|_| json!("zz")).collect();
        assert_eq!(
            validate(&json!({"items":{"enum":listed}}), &Value::Array(items)),
            Checked::Limit
        );
        let long = json!("a".repeat(1_000_000));
        assert_eq!(
            validate(&json!({"pattern":"(?:a|b)*a(?:a|b){30}c"}), &long),
            Checked::Limit
        );
        let strings: Vec<Value> = (0..16_000).map(|_| json!("ab".repeat(30))).collect();
        let patterns: Vec<Value> = (0..64)
            .map(|i| json!({"pattern": format!("(?:a|b)*a(?:a|b){{{}}}c", 20 + i % 10)}))
            .collect();
        assert_eq!(
            validate(&json!({"items":{"anyOf":patterns}}), &Value::Array(strings)),
            Checked::Limit
        );
    }

    /// Fix round 2 #1: `uniqueItems` over more than 20 items compares the
    /// items whose hashes collide, and each comparison is charged. Arrays
    /// hash as the flat sequence of their leaves, so `[P, c]` for every
    /// nesting `c` of `[1..7]` collide: 64 distinct items whose pairwise
    /// comparisons each walk the shared 1,000-node prefix `P`, about two
    /// million units, while the value has 64,000 nodes.
    #[test]
    fn colliding_unique_items_are_charged() {
        fn nestings(leaves: &[u64]) -> Vec<Value> {
            // Every split of `leaves` into consecutive runs, as arrays.
            if leaves.is_empty() {
                return vec![json!([])];
            }
            let mut all = Vec::new();
            for cut in 1..=leaves.len() {
                for rest in nestings(&leaves[cut..]) {
                    let mut runs = vec![json!(leaves[..cut])];
                    runs.extend(rest.as_array().unwrap().iter().cloned());
                    all.push(Value::Array(runs));
                }
            }
            all
        }
        let prefix: Vec<Value> = (0..1000).map(|i| json!(i)).collect();
        let items: Vec<Value> = nestings(&[1, 2, 3, 4, 5, 6, 7])
            .into_iter()
            .map(|nesting| json!([prefix, nesting]))
            .collect();
        assert_eq!(items.len(), 64);
        assert_eq!(
            validate(&json!({"uniqueItems":true}), &Value::Array(items)),
            Checked::Limit
        );
    }

    /// Fix round 2 #1: the names a `dependentRequired` entry lists are
    /// charged as they are scanned: 500 entries of 100 names over 300
    /// objects that have every name scan 15 million names.
    #[test]
    fn dependent_required_names_are_charged() {
        let names: Vec<String> = (0..100).map(|i| format!("k{i}")).collect();
        let mut required = serde_json::Map::new();
        for i in 0..500 {
            required.insert(format!("d{i}"), json!(names));
        }
        let object: serde_json::Map<String, Value> =
            names.iter().map(|name| (name.clone(), json!(1))).collect();
        let mut object = object;
        for i in 0..500 {
            object.insert(format!("d{i}"), json!(1));
        }
        let objects: Vec<Value> = (0..300).map(|_| Value::Object(object.clone())).collect();
        assert_eq!(
            validate(
                &json!({"items":{"dependentRequired":required}}),
                &Value::Array(objects)
            ),
            Checked::Limit
        );
    }

    /// The cases of `schema` whose answer is not the one wanted, as
    /// `(value, got)`.
    fn wrong(schema: &Value, cases: &[(Value, Checked)]) -> Vec<(Value, Checked)> {
        cases
            .iter()
            .filter_map(|(value, want)| {
                let got = validate(schema, value);
                (got != *want).then(|| (value.clone(), got))
            })
            .collect()
    }

    /// Critical r1 #6: `multipleOf` divides exactly on each number's
    /// shortest round-trip decimal: 0.3 is a multiple of 0.1. The
    /// JSON-Schema-Test-Suite's `multipleOf` cases keep their answers.
    #[test]
    fn multiple_of_is_decimal_exact() {
        use Checked::{Invalid, Valid};
        let cases: &[(Value, &[(Value, Checked)])] = &[
            (
                json!({"multipleOf":0.1}),
                &[(json!(0.3), Valid), (json!(0.35), Invalid)],
            ),
            (
                json!({"multipleOf":0.01}),
                &[(json!(19.99), Valid), (json!(-0.07), Valid)],
            ),
            (
                json!({"multipleOf":2}),
                &[
                    (json!(10), Valid),
                    (json!(7), Invalid),
                    (json!("foo"), Valid),
                ],
            ),
            (
                json!({"multipleOf":1.5}),
                &[(json!(0), Valid), (json!(4.5), Valid), (json!(35), Invalid)],
            ),
            (
                json!({"multipleOf":0.0001}),
                &[(json!(0.0075), Valid), (json!(0.00751), Invalid)],
            ),
            (
                json!({"type":"integer","multipleOf":0.123_456_789}),
                &[(json!(1e308), Invalid)],
            ),
            (
                json!({"type":"integer","multipleOf":1e-8}),
                &[(json!(12_391_239_123_u64), Valid)],
            ),
            (
                json!({"multipleOf":1e300}),
                &[(json!(5), Invalid), (json!(2e300), Valid)],
            ),
        ];
        let wrong: Vec<_> = cases
            .iter()
            .flat_map(|(schema, values)| {
                wrong(schema, values)
                    .into_iter()
                    .map(move |w| (schema.clone(), w))
            })
            .collect();
        assert!(wrong.is_empty(), "{wrong:?}");
    }

    /// Critical r1 #7: the four bounds compare integers exactly, and an
    /// integer with a float without rounding: 9007199254740993 and
    /// 9007199254740992 are distinct.
    #[test]
    fn numeric_bounds_compare_exactly() {
        use Checked::{Invalid, Valid};
        let (big, below) = (9_007_199_254_740_993_u64, 9_007_199_254_740_992_u64);
        let cases: &[(Value, &[(Value, Checked)])] = &[
            (
                json!({"minimum":big}),
                &[
                    (json!(below), Invalid),
                    (json!(big), Valid),
                    (json!(9_007_199_254_740_992.0), Invalid),
                ],
            ),
            (
                json!({"maximum":below}),
                &[(json!(big), Invalid), (json!(below), Valid)],
            ),
            (
                json!({"exclusiveMinimum":below}),
                &[
                    (json!(big), Valid),
                    (json!(below), Invalid),
                    (json!(9_007_199_254_740_992.0), Invalid),
                ],
            ),
            (
                json!({"exclusiveMaximum":big}),
                &[
                    (json!(below), Valid),
                    (json!(big), Invalid),
                    (json!(9_007_199_254_740_992.0), Valid),
                ],
            ),
            (
                json!({"minimum":1.1}),
                &[(json!(1), Invalid), (json!(2), Valid), (json!(1.1), Valid)],
            ),
            (
                json!({"maximum":-2}),
                &[(json!(-2.0001), Valid), (json!(-1.9999), Invalid)],
            ),
        ];
        let wrong: Vec<_> = cases
            .iter()
            .flat_map(|(schema, values)| {
                wrong(schema, values)
                    .into_iter()
                    .map(move |w| (schema.clone(), w))
            })
            .collect();
        assert!(wrong.is_empty(), "{wrong:?}");
    }

    /// Critical r1 #8: `uniqueItems` hashes numbers as it compares them,
    /// on both sides of its 20-item threshold: 0 and -0.0, and 1 and 1.0,
    /// are duplicates; 9007199254740993 and 9007199254740992.0 are not,
    /// and `const` agrees.
    #[test]
    fn unique_items_hashes_numbers_as_it_compares_them() {
        use Checked::{Invalid, Valid};
        let with = |a: Value, b: Value, more: u64| {
            let mut items = vec![a, b];
            items.extend((2..2 + more).map(|i| json!(i)));
            Value::Array(items)
        };
        let unique = json!({"uniqueItems":true});
        let mut cases = Vec::new();
        for more in [0, 20] {
            cases.push((with(json!(0), json!(-0.0), more), Invalid));
            cases.push((with(json!(1), json!(1.0), more), Invalid));
            cases.push((
                with(
                    json!(9_007_199_254_740_993_u64),
                    json!(9_007_199_254_740_992.0),
                    more,
                ),
                Valid,
            ));
        }
        let mut wrong = wrong(&unique, &cases);
        wrong.extend(wrong_const());
        assert!(wrong.is_empty(), "{wrong:?}");
    }

    /// `const` with an integer against a float of the nearest double.
    fn wrong_const() -> Vec<(Value, Checked)> {
        wrong(
            &json!({"const":9_007_199_254_740_993_u64}),
            &[
                (json!(9_007_199_254_740_992.0), Checked::Invalid),
                (json!(9_007_199_254_740_993_u64), Checked::Valid),
            ],
        )
    }

    /// Critical r1 #9 (C1 §4): the subschema and pattern limits and the
    /// regex program limit hold over every schema position, reached or
    /// not: 2,050 unused `$defs` schemas, 65 unused patterns, and one
    /// unused pattern over the program limit are each refused.
    #[test]
    fn limits_count_unused_schema_positions() {
        let schemas: serde_json::Map<String, Value> = (0..2050)
            .map(|i| (format!("d{i}"), json!({"type":"integer"})))
            .collect();
        let patterns: serde_json::Map<String, Value> = (0..65)
            .map(|i| (format!("d{i}"), json!({"pattern": format!("^a{i}$")})))
            .collect();
        let big = json!({"big": {"pattern": "(?:a{1000}){1000}"}});
        let admitted: Vec<&str> = [
            ("2,050 unused schemas", json!({"$defs": schemas})),
            ("65 unused patterns", json!({"$defs": patterns})),
            (
                "an unused pattern over the program limit",
                json!({"$defs": big}),
            ),
        ]
        .iter()
        .filter(|(_, schema)| compiles(schema))
        .map(|(probe, _)| *probe)
        .collect();
        assert!(admitted.is_empty(), "admitted: {admitted:?}");
        // Within the limits, unused positions compile.
        let defs: serde_json::Map<String, Value> = (0..64)
            .map(|i| (format!("d{i}"), json!({"pattern": format!("^a{i}$")})))
            .collect();
        assert!(compiles(&json!({"$defs": defs})), "64 unused patterns");
    }

    /// Critical r2 #4 (C1 §4): a reference target outside the schema
    /// keywords, which compiling promotes to a schema, is held to the same
    /// limits over every position under it: 2,050 booleans, 65 patterns,
    /// and one unused pattern over the program limit, each under
    /// `{"$ref":"#/x","x":…}`, are refused.
    #[test]
    fn limits_count_promoted_reference_targets() {
        let booleans: serde_json::Map<String, Value> =
            (0..2050).map(|i| (format!("d{i}"), json!(true))).collect();
        let patterns: serde_json::Map<String, Value> = (0..65)
            .map(|i| (format!("d{i}"), json!({"pattern": format!("^a{i}$")})))
            .collect();
        let big = json!({"big": {"pattern": "(?:a{1000}){1000}"}});
        let admitted: Vec<&str> = [
            ("2,050 booleans", Value::Object(booleans)),
            ("65 patterns", Value::Object(patterns)),
            ("a pattern over the program limit", big),
        ]
        .into_iter()
        .filter(|(_, defs)| compiles(&json!({"$ref": "#/x", "x": {"$defs": defs}})))
        .map(|(probe, _)| probe)
        .collect();
        assert!(admitted.is_empty(), "admitted: {admitted:?}");
    }

    /// Critical r2 #4: each schema position counts once. 2,048 positions
    /// compile, whether a `$defs` entry is referenced or a promoted
    /// target is reached both inside and whole; 2,049 do not.
    #[test]
    fn each_position_counts_once() {
        let defs = |n: usize| -> serde_json::Map<String, Value> {
            (0..n).map(|i| (format!("d{i}"), json!(true))).collect()
        };
        // The root, its `$defs` entries.
        let referenced = |n| json!({"$ref": "#/$defs/d0", "$defs": defs(n)});
        assert!(compiles(&referenced(2047)), "a referenced $defs entry");
        assert!(!compiles(&referenced(2048)), "2,049 positions");
        // The root, two `allOf` items, `x`, its `$defs` entries.
        let promoted = |n| {
            json!({"allOf": [{"$ref": "#/x/$defs/d0"}, {"$ref": "#/x"}],
                   "x": {"$defs": defs(n)}})
        };
        assert!(
            compiles(&promoted(2044)),
            "a target inside a promoted target"
        );
        assert!(!compiles(&promoted(2045)), "2,049 positions with a target");
    }

    /// Critical r1 #1: converting a pattern's ECMA escapes is linear. A
    /// 24 KiB pattern of `\d` took over 8 s to compile before (release);
    /// it now compiles at once.
    #[test]
    fn repeated_pattern_escapes_compile_in_linear_time() {
        let schema = json!({"pattern": "\\d".repeat(12 * 1024)});
        let start = std::time::Instant::now();
        assert!(compiles(&schema));
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "took {elapsed:?}"
        );
    }

    /// Critical r1 #1: the bytes a pattern converts to are charged to the
    /// compile budget before its regular expression is built. 64
    /// `patternProperties` names of `\w` at the 256 KiB cap convert to
    /// about 1.2 MB, each within the regex program limit: built, they took
    /// about 150 ms (release); charged, they are refused first.
    #[test]
    fn converted_pattern_bytes_are_charged() {
        let names: serde_json::Map<String, Value> = (0..64)
            .map(|i| (format!("{i}{}", "\\w".repeat(1360)), json!(true)))
            .collect();
        let schema = json!({"patternProperties": names});
        assert!(schema.to_string().len() <= 256 * 1024);
        assert!(!compiles(&schema));
    }

    /// Fix round 3 #1: a `$dynamicRef` charges its anchor name's bytes,
    /// for its comparison with the target's `$dynamicAnchor` and for each
    /// scope its resolution looks the name up in. A 100,000-byte anchor
    /// reached from 2,000 items is about 1,563 units per comparison and
    /// per lookup: past the budget, where one unit each was not.
    #[test]
    fn dynamic_anchor_names_are_charged_by_length() {
        let name = "a".repeat(100_000);
        let schema = json!({
            "$dynamicAnchor": name,
            "items": {"$dynamicRef": format!("#{name}")},
        });
        assert!(compiles(&schema), "the schema compiles");
        let value = Value::Array((0..2000).map(|i| json!(i)).collect());
        assert_eq!(validate(&schema, &value), Checked::Limit);
    }

    /// A schema that walks `objects` levels of `{"a":…}`, one `$defs`
    /// schema per level, and at the last level checks the property name
    /// through a chain of `chain` `allOf` references: the name's
    /// evaluations nest inside the object's.
    fn nesting_schema(objects: usize, chain: usize) -> Value {
        let mut defs = serde_json::Map::new();
        for i in 0..objects {
            defs.insert(
                format!("o{i}"),
                json!({"properties":{"a":{"$ref": format!("#/$defs/o{}", i + 1)}}}),
            );
        }
        defs.insert(
            format!("o{objects}"),
            json!({"propertyNames":{"$ref":"#/$defs/c0"}}),
        );
        for i in 0..chain {
            defs.insert(
                format!("c{i}"),
                json!({"allOf":[{"$ref": format!("#/$defs/c{}", i + 1)}]}),
            );
        }
        defs.insert(format!("c{chain}"), json!({"type":"string"}));
        json!({"$ref":"#/$defs/o0","$defs":defs})
    }

    /// Fix round 2 #2: `propertyNames` evaluations nest inside the
    /// evaluation that reached them and count against the same depth of
    /// 512. Each chain alone stays within it, together they do not; just
    /// within it, the nesting runs on the validation thread's stack.
    #[test]
    fn nested_evaluations_share_one_depth() {
        // About 2 levels per object and 2 per chain link.
        assert_eq!(
            validate(&nesting_schema(1, 200), &nested(2)),
            Checked::Valid
        );
        assert_eq!(
            validate(&nesting_schema(200, 1), &nested(201)),
            Checked::Valid
        );
        assert_eq!(
            validate(&nesting_schema(120, 120), &nested(121)),
            Checked::Valid
        );
        assert_eq!(
            validate(&nesting_schema(200, 200), &nested(201)),
            Checked::Limit
        );
    }

    /// Fix round 1 #1: a value at C1's maximum (65,536 nodes, depth 64)
    /// against a moderately complex schema fits the budget.
    #[test]
    fn a_realistic_maximal_value_fits() {
        let schema = json!({
            "type":"object","required":["items","tree"],"additionalProperties":false,
            "properties":{"items":{"type":"array","items":{"$ref":"#/$defs/item"}},
                          "tree":{"$ref":"#/$defs/node"}},
            "$defs":{
                "item":{"type":"object","required":["id","name","kind","tags"],
                    "additionalProperties":false,
                    "properties":{"id":{"type":"integer","minimum":0},
                        "name":{"type":"string","pattern":"^[a-z][a-z0-9_-]{0,63}$"},
                        "kind":{"enum":["file","dir","link","note","task","bug","doc","test"]},
                        "tags":{"type":"array","uniqueItems":true,
                                "items":{"type":"string","minLength":1}},
                        "meta":{"anyOf":[{"type":"null"},{"type":"object",
                            "properties":{"line":{"type":"integer"},"col":{"type":"integer"}}}]}}},
                "node":{"type":"object","properties":{"a":{"$ref":"#/$defs/node"},
                                                      "v":{"type":"integer"}}}}
        });
        let items: Vec<Value> = (0..5000)
            .map(|i| {
                json!({"id":i,"name":format!("n{i}"),"kind":"task","tags":["a","b"],
                            "meta":{"line":i,"col":1}})
            })
            .collect();
        let tree = (0..61).fold(json!({"v":1}), |inner, _| json!({"a":inner,"v":1}));
        let value = json!({"items":items,"tree":tree});
        assert_eq!(validate(&schema, &value), Checked::Valid);
    }

    /// Fix round 2 #3: the metaschema check of compiling is bounded. Each
    /// `$ref` to a location that is not yet a subschema checks the value
    /// there against the metaschema; refs taken deepest first into a chain
    /// of 28 `allOf` levels re-check the 50,000-item `enum` at its end for
    /// every level, about 1.4 million evaluations for one 250 KiB schema
    /// with 60 subschemas. It does not compile; the plain chain does.
    #[test]
    fn the_metaschema_check_is_bounded() {
        let levels = 28;
        let payload: Vec<Value> = (0..50_000).map(|i| json!(i)).collect();
        let chain = (0..levels).fold(
            json!({ "enum": payload }),
            |inner, _| json!({"allOf":[inner]}),
        );
        let refs: Vec<Value> = (0..=levels)
            .rev()
            .map(|depth| json!({"$ref": format!("#/enum/0{}", "/allOf/0".repeat(depth))}))
            .collect();
        assert!(compiles(&json!({ "enum": [chain.clone()] })));
        assert!(!compiles(&json!({"enum":[chain],"anyOf":refs})));
    }

    /// Fix round 1 #1: compiling is bounded too: too many subschemas, or
    /// too many patterns, do not compile.
    #[test]
    fn compile_limits() {
        let mut defs = serde_json::Map::new();
        for i in 0..3000 {
            defs.insert(
                format!("d{i}"),
                json!({"$ref": format!("#/$defs/d{}", (i + 1) % 3000)}),
            );
        }
        assert!(!compiles(&json!({"$ref":"#/$defs/d0","$defs":defs})));
        let mut patterns = serde_json::Map::new();
        for i in 0..65 {
            patterns.insert(format!("^p{i}$"), json!(true));
        }
        assert!(!compiles(&json!({ "patternProperties": patterns.clone() })));
        patterns.remove("^p64$");
        assert!(compiles(&json!({ "patternProperties": patterns })));
    }
}
