# W4: last Task 1 fixes after the W3 Sol reviews

W3-F and W3-G are merged. GPT-6 Sol medium reviewed each branch
(`../w3/sol-review-W3-F.md`: unsound; `../w3/sol-review-W3-G.md`: sound
with changes). W4-H and W4-I run in parallel; the orchestrator merges both.

Rules: `../w1/common.md`, except reports go to `reports/<task id>.md` here.

| Task | Findings | Model | Brief |
|---|---|---|---|
| W4-H | W3-F 1, 2, 4 (force race, force evidence, join bound) | Opus 5.5 high | `h.md` |
| W4-I | W3-G 1, 4, 5 (store_error data, bounded set, deadline class) | Opus 5.5 medium | `i.md` |

Moved to `via-jm4.7.7` (Task 3 owns F12, the Store-failure lifecycle):
W3-G 2 (Store failure → daemon-wide health, stopped admission, prompt
cleanup), W3-G 3 (Store operation bound on every reply wait), W3-F 3
(cancel lifecycle events after a Store failure; reconcile the head, then
commit them or report the terminal unpersisted).
