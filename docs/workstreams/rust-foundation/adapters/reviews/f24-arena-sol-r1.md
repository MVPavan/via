**UNSOUND as submitted:** the test change is correct and the allocator choice is defensible, but the authoritative gate needs durable integration. No Blocker found.

- **Important — [runtime-contracts.md:1195](../../../../../docs/specs/runtime-contracts.md#L1195): authoritative musl gate is absent from committed verification commands.** [.repo-context/verification.md:46](../../../../../.repo-context/verification.md#L46) still runs F24 for the host target. Future verification can pass the glibc proxy without exercising the declared authority. **Smallest fix:** add the musl F24 command and prerequisites to the committed verification guide: build the musl fake agent, run nonempty F24 with `test-failpoints` for `x86_64-unknown-linux-musl`, and require complete evidence and a successful exit. A new gate script is unnecessary.

- **Minor — [runtime-contracts.md:1189](../../../../../docs/specs/runtime-contracts.md#L1189), [s1_f24_memory.rs:61](../../../../../crates/via-cli/tests/s1_f24_memory.rs#L61): causal wording exceeds the evidence.** Interleaved measurements strongly support allocator-sensitive RSS growth; they do not prove “without any VIA retention,” identify exactly which freed buffers remain resident, or establish two-arena glibc as equivalent to musl. “Passes every run” should identify the measured sample. **Smallest fix:** say the results are consistent with allocator retention/fragmentation, report the observed pass counts, and describe two arenas as an empirical development proxy.

The choice otherwise fits §8, packaging §1 and invariants: both limits remain unchanged, the shipped Linux target is musl-static, and this introduces no production memory setting.

Two arenas cannot reclaim allocations still owned by VIA. The test retains its RSS assertions, but **does not preserve identical sensitivity**: reducing allocator overhead can let small leaks fit existing RSS headroom, and changed allocator contention can change whether timing-dependent defects reproduce. It also removes the default-glibc fragmentation signal. Those limits are acceptable for a development proxy when musl remains mandatory; this is not proof of absence of retention. [glibc documents arena control as limiting arenas](https://sourceware.org/glibc/manual/latest/html_node/Memory-Allocation-Tunables.html).

The implementation checks out:

- `cfg!(target_env = "gnu")` selects the intended Linux GNU build; musl selects `None`. The test and daemon use the same Cargo target.
- `start_with` applies the setting **after** `Sandbox::command` clears the environment and **before** spawn.
- `rss.json` correctly records `"2"` or `null`.
- Anchor launches clear their environment; fake-vendor launch uses an explicit allow list excluding this variable. Client commands and other tests receive no change.

Verified: supplied diff matches the scoped working-tree diff; formatting, diff whitespace and relative-link checks pass. The supplied log records 8 glibc and 2 musl passes plus clean Clippy. Git status is unchanged.

Could not verify: the coordinator’s gate implementation, fresh F24 execution, leak-injection sensitivity, or release-musl performance. Historical archive experiments returned `rc=100` because evidence collection lacked `.git`; their memory measurements support diagnosis, but are not complete gate passes.