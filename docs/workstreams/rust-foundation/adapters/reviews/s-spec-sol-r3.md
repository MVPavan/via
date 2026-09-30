**UNSOUND**

| Item | Fixed, partly or not | Reason | File:line |
|---|---|---|---|
| #3 | Fixed | Version facts and `vendor_version_untested` now follow the described turn. | `docs/specs/via-api-v1.md:367`, `:371` |
| #14 | Fixed | Nonempty final text that fails JSON parsing explicitly produces `structured_output_invalid`. | `docs/specs/vendors/codex.md:161` |
| OpenCode creation/reopen | Fixed | Initial creation uses POST; a stored ID routes to exact-ID readback, with no replacement creation. | `docs/specs/vendors/opencode.md:493`, `:495` |

**New defect:** Codex’s replacement rule is unconditional: *all* nonempty final text becomes structured output, and unparsable text fails. With `output_schema:null`, an ordinary answer such as `Done` would therefore fail despite the schema being cleared (`docs/specs/via-api-v1.md:503`). This broadens the previous rule, which only classified JSON-parsable text as structured output. Smallest fix: qualify the new parsing/validation requirement with “when an output schema is requested.” Location: `docs/specs/vendors/codex.md:161–164`.

**Could not verify:** Runtime behavior, private probe evidence, or live qualification. This was a text-only fix check. Diff whitespace check passed; Git status was clean. No files or Git state changed; no `bd`, vendor CLI, or model ran.