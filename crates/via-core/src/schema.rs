//! C1 Q2 structured-output validation (adapter design §5.1 #37, K14): a
//! frozen `output_schema` compiled as JSON Schema draft 2020-12 with the
//! vendored boon (`third_party/boon/VIA-PATCH.md`). The compiler is given
//! no URL loader, so a schema that refers to anything outside itself, a
//! file or a network address, does not compile; one that declares another
//! draft anywhere does not compile either. Compiling and validating are
//! bounded (fix round 1 #1), and both run off the async executor.
//!
//! The bounds were sized by measurement (release build): validating a
//! value at C1's maximum (65,536 nodes, depth 64) against a moderately
//! complex schema takes about 333,000 units and 3 ms; the slowest
//! adversarial case spends the whole budget in about 55 ms with peak
//! memory under 25 MiB; compiling stays under about 80 ms.

use boon::{Compiler, Draft, SchemaIndex, Schemas, SchemeUrlLoader, Validation};
use serde_json::Value;

/// The location the one schema is compiled under; nothing is loaded from it.
const LOCATION: &str = "urn:via:output_schema";

/// Most subschemas one `output_schema` compiles to.
const SUBSCHEMAS: usize = 2048;

/// Most `pattern` and `patternProperties` expressions in one schema.
const PATTERNS: usize = 64;

/// Work units one validation may spend (boon patch: one per subschema
/// evaluation, more for keywords whose work grows with the value).
const UNITS: u64 = 1_000_000;

/// Deepest evaluation nesting one validation may reach.
const DEPTH: usize = 512;

/// Stack of the thread that runs a compile or a validation: room for
/// [`DEPTH`] nested evaluations in an unoptimized build.
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
