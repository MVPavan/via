**UNSOUND.** At `98893ff`, 16 original findings are fixed; #3 and #14 remain partly fixed. The #8 fix introduces a creation/reopen ambiguity.

Locations below are current lines under `docs/specs/`: C1 = `via-api-v1.md`; C2 = `adapter-contract.md`; RT = `runtime-contracts.md`; CC/CX/OC = `vendors/claude-code.md`, `codex.md`, `opencode.md`.

| Finding | Disposition | One-line reason | File:line |
|---|---|---|---|
| #1 | Fixed | Compatible resume advances the recorded version; compatibility refusal is listed. | C1:20,235 |
| #2 | Fixed | Pin directive is superseded; `allow_untested` explicitly has no effect. | CX:190,561 |
| #3 | Partly fixed | Required facts exist, but latest-turn version facts can hide the selected turn’s unchecked-version warning. | C1:332,365 |
| #4 | Fixed | `DescribeRequest.effort` supplies pure spawn validation; the `TurnParams` row matches `check_turn`. | C2:153,190,196 |
| #5 | Fixed | Typed `model_list` is included in the permitted route surface. | CX:70 |
| #6 | Fixed | All three configuration descriptions admit adapter-owned `harnesses` settings and their startup/freeze rules. | RT:841,1063,1134 |
| #7 | Fixed | Durable warnings support optional structured `data.categories`. | C1:647 |
| #8 | Fixed | Both passages prohibit vendor I/O during logical open; OC’s replacement introduces the ambiguity below. | OC:493; CC:506 |
| #9 | Fixed | Close consistently cancels, disposes, performs bounded fallback, preserves history and commits the result. | OC:121,381,498,678; C2:639 |
| #10 | Fixed | Production raw capture is replaced by bounded read/count/discard; credential prohibitions and test evidence requirements survive. | CX:362,472,485; OC:147,642,753; RT:1082 |
| #11 | Fixed | Group cleanup and vendor-reported tool-item cleanup are distinguished correctly. | C2:777; CX:427; RT:40 |
| #12 | Fixed | Private/shared outcomes, acknowledged wall cleanup and the wall-capped grace window match AD4. | C1:266,280; C2:469,770; CX:421; OC:768 |
| #13 | Fixed | The stale usage proposal is superseded by keyed samples with turn scope and child-session exclusion. | OC:813 |
| #14 | Partly fixed | CC is corrected; CX still leaves nonempty final text that fails JSON parsing without an explicit invalid-output disposition. | CC:272,458; CX:161 |
| #15 | Fixed | Qualification now measures maximum admitted concurrent turns under per-connection admission. | CX:386; RT:1145 |
| #16 | Fixed | Nullable `leftovers` is always present; qualifying reports alone are non-null. | C1:303,591,622 |
| #17 | Fixed | Start-tick ordering, timestamp derivation, accuracy and precision are restored without selecting detection. | C2:504; C1:622 |
| #18 | Fixed | The status reference correctly points to C1 §3.7. | CC:487 |

New defects introduced by the fixes:

| Severity | Location | Defect | Evidence | Smallest fix |
|---|---|---|---|---|
| Important | C1:365–373 | Version fields and warnings use the latest turn rather than the selected turn. | C1:332 allows selecting an older turn; AD7:645 requires its unchecked-version warning in status. After untested turn 1 and tested turn 2, `status(turn=1)` can show tested/no warning. | Derive vendor version/status and version warning from the selected turn; retain the explicitly session-scoped adapter version and frozen inheritance states. |
| Important | OC:493 | The new instruction prescribes `POST /session` in the first turn of every connection generation without distinguishing creation from reopening. | OC:495 requires exact-ID readback on idle reopen; AD3:387 distinguishes creation from reopening. The new wording permits conflicting recipes after retirement. | Restrict POST to initial creation; use exact stored-ID readback on reopening, within that generation’s first `run_turn`. |

The status field names, required-field presentation, nullable `vendor_version`, inheritance enum and Codex default example otherwise fit C1 and AD12/AD13. For #14, explicitly classify nonempty unparsable output as `structured_output_invalid`; reserve missing-output handling for absent output on a completed terminal.

OC:720’s additional change is sound: bounded vendor data preserves the reported accounting value without requiring production raw logging.

All specified deferred conflict-4 passages remain byte-for-byte unchanged. Credential prohibitions, CX raw-span evidence, OC acceptance artifacts and runtime test-supervisor evidence rules survived.

Could not verify vendor behavior, private probe evidence, live qualification or implementation correctness. No files or Git state were changed; no `bd`, vendor CLI or model was run. Changed-file relative links and diff whitespace checks passed; final Git status is clean. The two known out-of-scope issues were excluded.