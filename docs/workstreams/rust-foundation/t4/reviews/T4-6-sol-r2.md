SOUND

### Findings

None in `git diff 6d18e26..HEAD`. The five fixes are present. The merge retains `ApiError.floor`, the `describe` and `models` arms, and all seven T4-6/T4-7 release failpoints. Receipt size checks precede `unknown_model`; the envelope maximum test still constructs and checks a 1 KiB model. T4-6 daemon starts use the direct socket, child PID checked probe, and the scenarios do not read daemon traces or shutdown summaries from stderr.

F24’s 32–64 MiB peak compared with the later peak excludes the documented dip at 64 MiB without concealing a higher post-threshold peak. The 282 MiB flood gives the sampler margin above 256 MiB. The reported residual slope alone does not establish allocation missing from §5.1.

The full failpoint suite passed **521/521** tests. The focused cross-crate selection passed **16/16**; formatting, failpoint-enabled clippy, the release feature check, and `git diff --check` passed. Git status is clean.

### Could not verify

The requested old-code failure proof is **not established** for the F24 metric, PID readiness, or first-item `debug_assert!`: the available tests do not deterministically fail on those earlier implementations. Archived failing runs do establish that the `failure.message`, oversized `events.after`, list-cursor, and unwired `final_text.write.fail` tests catch their respective faults. I did not rerun old revisions or independently inspect the raw samples behind the reported 38 F24 runs.
