//! C1 Q2 structured-output validation (adapter design §5.1 #37, K14): a
//! frozen `output_schema` compiled as JSON Schema draft 2020-12. The
//! compiler is given no URL loader, so a schema that refers to anything
//! outside itself, a file or a network address, does not compile.

use boon::{Compiler, Draft, SchemaIndex, Schemas, SchemeUrlLoader};
use serde_json::Value;

/// The location the one schema is compiled under; nothing is loaded from it.
const LOCATION: &str = "urn:via:output_schema";

/// A compiled `output_schema`.
pub(crate) struct Validator {
    schemas: Schemas,
    index: SchemaIndex,
}

impl Validator {
    /// `schema` compiled as draft 2020-12 (C1 §4), or `None` when it is not
    /// a valid schema or refers outside itself.
    pub(crate) fn compile(schema: &Value) -> Option<Self> {
        let mut compiler = Compiler::new();
        compiler.set_default_draft(Draft::V2020_12);
        compiler.use_loader(Box::new(SchemeUrlLoader::new()));
        compiler.add_resource(LOCATION, schema.clone()).ok()?;
        let mut schemas = Schemas::new();
        let index = compiler.compile(LOCATION, &mut schemas).ok()?;
        Some(Self { schemas, index })
    }

    /// Whether `value` is valid against the schema.
    pub(crate) fn accepts(&self, value: &Value) -> bool {
        self.schemas.validate(value, self.index).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::Validator;

    /// Draft 2020-12 rules apply; an invalid schema and one that refers
    /// to a file or the network do not compile.
    #[test]
    fn schemas_compile_only_self_contained_and_valid() {
        let schema = json!({"type":"object","properties":{"a":{"type":"integer"}},
                            "required":["a"],"dependentRequired":{"a":[]}});
        let validator = Validator::compile(&schema).unwrap();
        assert!(validator.accepts(&json!({"a":1})));
        assert!(!validator.accepts(&json!({"a":"1"})));
        assert!(!validator.accepts(&json!({"b":1})));
        assert!(Validator::compile(&json!({"type":5})).is_none());
        for outside in ["file:///etc/passwd", "https://example.com/schema.json"] {
            assert!(
                Validator::compile(&json!({"$ref": outside})).is_none(),
                "{outside}"
            );
        }
    }
}
