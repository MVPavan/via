## Six findings

1. **RESOLVED** — §3 and §6 make Claude and Codex the executable v0 set; OpenCode spawn remains refused pending a verified external sandbox ([lines 222, 445–455](../access-methods.md)).
2. **RESOLVED** — §2 and §5 now say ACP permission requests are optional and identify where write and network bounds must come from ([lines 193, 420–425](../access-methods.md)).
3. **RESOLVED** — Droid’s `add_user_message` is described as sending a turn; mid-turn steer is UNVERIFIED and withheld pending a probe ([lines 227, 315–320](../access-methods.md)).
4. **RESOLVED for Pi** — Cancel now clears the queue, aborts, checks settlement, and names the race and forced-kill fallback ([lines 338–346](../access-methods.md)).
5. **RESOLVED** — §3 requires one route per managed run, and the Gemini and Hermes rows select routes at spawn ([lines 202–207](../access-methods.md)).
6. **RESOLVED** — RPC concurrency is vendor-specific, with one Pi process budgeted per simultaneous run ([lines 53–65](../access-methods.md)).

## New issues

- **MAJOR — §6, lines 492–494:** The Pi cancel sequence is applied to **Oh My Pi** as fact. [OMP’s RPC reference](https://github.com/can1357/oh-my-pi/blob/main/docs/rpc.md) lists no `clear_queue` and uses terminal `agent_end`, not Pi’s `agent_settled`. Specify OMP cancel separately and probe it.
- **MINOR — §6, lines 453–459:** The proposed `bwrap` bound is described as making only node grants writable. The [actual mount policy](https://github.com/MVPavan/coding-ritual/blob/09cee1bac75cf2ec6c23839ba3f1c069be80b379/workflow_interpreter/inspector/sandbox.py#L1) also leaves `$HOME`, `/tmp`, channels, and some Git state writable. State the bound’s scope and test the stated “cannot write” promise against that scope.

**Verdict: ACCEPT WITH FIXES.** The six original concerns are addressed; the two new claims need correction. Read-only review; no files changed.