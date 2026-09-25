# Skill packaging

Keep one canonical directory per workflow with `SKILL.md`; add references only
for substantial conditional material and scripts for reusable mechanical work.
Inline the common contract. Avoid mandatory chains of references.

## Frontmatter and invocation

Require `name` matching the directory and a concise `description` naming the
activation boundary. Put important exclusions near the positive trigger. A short
outcome phrase is acceptable when it improves selection; do not duplicate the
whole workflow or assume any wording technique guarantees invocation behavior.

Use `disable-model-invocation: true` for manual workflows and compatibility
aliases. The paired Codex policy is `agents/openai.yaml` →
`policy.allow_implicit_invocation: false`. This repository's catalog generator
updates that sidecar and the router together. Actual catalog exposure and context
loading depend on the harness; inspect discovery instead of assuming zero cost.

A manual entrypoint can delegate to a shared workflow when explicitly requested.
Keep its body thin and preserve the user's authority boundaries. Other skills
should select the canonical owner rather than automatically activating aliases.

## Cross-references and verification

Name the owner and the condition for reading a reference. Use relative links for
nearby skill material and repository paths for project policy. Keep model IDs,
reasoning controls and tool-interface differences in runtime integration.

After metadata or routing edits, regenerate the catalog/sidecars and run the
catalog check. Check caller references and existing integration links before
removing an entrypoint. Compatibility pointers are appropriate when an authorized
edit scope excludes those integration files. Structural validity does not prove
behavioral routing; test representative positive and negative tasks when feasible.
