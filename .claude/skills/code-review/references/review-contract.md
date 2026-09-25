# Dispatched review contract

Inputs: mode, brief/requirements path, implementation report, scoped diff package,
binding constraints, output path. For re-review also include the finding list and
fix-delta package. Follow the code-review entrypoint's evidence and scope rules.

Initial spec and quality verdicts remain distinct when those roles are dispatched
separately. A combined reviewer covers both explicitly. Re-review covers every
listed finding regardless of its original role.

## Spec output

```text
Verdict: COMPLIANT | ISSUES_FOUND
Unverified requirements: <items and needed evidence, or none>
Issues: <severity, file:line, missing/extra/misunderstood requirement and impact>
Checks and limitations: <actual evidence>
```

A missing brief limits compliance claims; an unrelated failed check is not an
automatic Critical code defect. Report each limitation at its actual consequence.

## Quality output

```text
Verdict: APPROVE | WARNING | BLOCK — <reason>
Issues: <Critical / Important / Minor; file:line, trigger and impact>
Checks and limitations: <actual evidence>
```

## Re-review output

```text
Finding verdicts: <ID — ADDRESSED | NOT ADDRESSED; file:line evidence>
New breakage in fix diff: <severity and evidence, or none>
Out-of-scope observations: <nonblocking follow-ups, or none>
Verdict: all addressed, no new Critical/Important | findings remain open
Checks and limitations: <actual evidence>
```

A reviewer may report that a finding was incorrect; the coordinator records that
ruling and its evidence separately from an implemented fix. Uncertain findings
remain explicitly unresolved. Do not demand edits solely to make a verdict green.
