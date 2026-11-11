# Results from real-model runs

Newest first. Each entry: date, model and API, what was run, what happened, and what changed because of it.

## 2026-11-11 — muse-spark-1.3-contributor via OpenAI-compatible gateway

Machine: Linux, unprivileged user namespaces blocked by AppArmor (`kernel.apparmor_restrict_unprivileged_userns=1`),
not a container. Modes `ask` and `auto` (dry mode would block unclassified commands without the offline sandbox).

| Step | Result |
|---|---|
| 1. API | Tool calls work; `rune verify` 2 requests, 0 differ. |
| 2. Fix with checks | Read before editing, fixed `add`, check 2 passed 0 failed; `verify` 7 requests, 0 differ. |
| 3. Outside edit + rewind | The model read the human's change and kept the comment. `/rewind` kept the comment and `sub`, removed the docstrings; afterwards the model read the restored file and described the state correctly. `/undo` restored the docstrings. `verify` 25 requests, 0 differ. |
| 4. Trap | The model removed the timeout condition in `client.py` instead of fixing `config.py`; the old test accepted it (2 passed, 0 failed), so neither rollback nor self-rewind was exercised. `verify` 8 requests, 0 differ. |

What it answered: a restored world plus the harness note was enough for this model to give a correct account of
the disk after a rewind (the design doc's main open question), at least on a small task.

Changed because of it:

- The trap now has a contract test and an explicit `rune-score:`; bypassing the timeout check scores 0, so it is
  a regression and triggers rollback. TESTING.md step 4 uses `--rollback-after 1`.
- Cache share showed 0% because the gateway's `toChatUsage()` drops the cached-token field from chat/completions
  responses. rune reads both `prompt_tokens_details` and `input_tokens_details`; the gateway needs to pass one of
  them through for the number to mean anything.
- Search works without user namespaces (directory-copy forks).
