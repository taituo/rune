# Development plan

What I build next, in order. Each milestone has deliverables, how it is accepted (unit tests here, and the
real-model tests from [TEST-PLAN.md](TEST-PLAN.md) by id), and the risks I expect. The order follows one rule:
**first make it trustworthy in daily use, then make it measurable, then make it clever.** Findings from the test
plan reorder this list; results go to [results.md](results.md), state to [TODO.md](TODO.md).

Working rules for myself:

- Every change lands with a test that fails without it. Push only after a clean full `cargo test` run.
- Measure before and after anything called an optimisation; put the numbers in the commit message.
- Name known gaps in README "Known limits" the moment they exist, not later.
- Keep `rune verify` exact: any change to rendering or folding must keep old sessions re-rendering.

---

## M1 — Daily-use robustness (first; unblocks T1–T3)

| # | Deliverable | Accepted by |
|---|---|---|
| M1.1 | **Ctrl-C interrupts a turn cleanly**: the running tool's process group is killed, a `tool.post` with "Interrupted by the user" is recorded, the chat prompt returns. Second Ctrl-C exits. | unit test with a sleeping Bash call and a signal; T1.5 |
| M1.2 | **Incremental fold**: cache folded messages per branch head; fold only new events; full refold on rewind. Today a step costs O(events). | `verify_requests` still exact; `scale` example flat per-step cost to 2000 steps; T8.2 |
| M1.3 | **Context budget, step 1**: count prompt tokens per request (from usage); when a request passes a threshold, elide old tool results to blob references (`[read src/x.rs @4b1d, 240 lines — use ReadAt]`) and add a `ReadAt` tool that returns the exact stored bytes. Rendering stays deterministic (elision decisions are recorded as events). | verify exact across elision; long-session test; T1.1 at 50+ turns |
| M1.4 | **Gateway quirks**: tool calls without ids, duplicate ids across turns, `arguments` as an object instead of a string, empty `content` with tool calls, `finish_reason` missing. Each becomes a mock-server test. | `tests/openai.rs` grows one test per quirk; T1.2 |
| M1.5 | **max_tokens continuation**: when a response stops on max_tokens mid-text, continue it once; mid-tool-call, tell the model its call was cut. | mock test |
| M1.6 | **Two sessions, one worktree**: lock the shadow index per worktree (flock), or give each session its own index file. | concurrency test with two writers; T2.7 |
| M1.7 | **Symlink-safe file tools**: resolve a path and refuse writes whose real path leaves the worktree (or, in a fork, the fork). | test with a symlink to `/tmp` and to the main path from a fork |
| M1.8 | **`rune doctor`**: one command that checks git version, namespaces, overlay support, gateway reachability, model list, write access to RUNE_HOME, and prints what will and will not work on this machine. | manual; replaces the "if unshare fails" paragraph in TESTING.md |

Risks: Ctrl-C handling around blocking reads of the SSE stream (needs a cancel flag and a read timeout); elision
changes the prompt prefix, so it must happen rarely and at turn boundaries to keep caching useful.

## M2 — Measurement (`rune eval`; turns T9 into a command)

| # | Deliverable | Accepted by |
|---|---|---|
| M2.1 | **Task format**: JSON files under `eval/tasks/`: repo path, base commit, prompt, visible check, hidden test files + command, timeout. | parser tests |
| M2.2 | **Task miner**: `rune eval mine <repo>` walks history for commits that touch both code and tests; produces tasks where the base is the parent commit with the commit's *tests* held out. | run on rune's own history: ≥ 5 tasks |
| M2.3 | **Runner**: `rune eval run <tasks> --modes plain,checks,rollback,search --runs 2` in isolated copies; parallel up to N; resumable. | scripted-provider run over 3 tasks |
| M2.4 | **Report**: JSON + Markdown table: pass rate on hidden tests, steps, tokens, wall time, rewinds per mode, with per-task drill-down links to branches. | report snapshot test |
| M2.5 | **Experiments from the design doc** as eval variants: (a) rewind with restored world vs conversation-only; (b) planted wrong lesson; (c) elision on/off with action agreement. | each runs on scripted + real model |

Risks: real tasks need their dependencies installed in each copy (start with Python/Rust projects with no
network needs); model nondeterminism needs enough runs per cell to mean anything (start with 3, report spread).

## M3 — Isolation and speed

| # | Deliverable | Accepted by |
|---|---|---|
| M3.1 | **Scan world in-process**: own index with a stat cache (mtime, size, inode, mode) so a snapshot stats files and hashes only changed ones, no `git add` subprocess. Same tree as git for the same files (property test against `git write-tree`). | equality tests; bench: < 30 ms warm on 20k files |
| M3.2 | **Interactive sessions on overlay** (env B): the agent works in its layer; at turn end its changes merge into the live tree keep-human; your edits reach it at the next turn. Opt-in flag first. | attribution tests become exact (no `unknown` inside turns); T2.2 |
| M3.3 | **Faster copy forks** (env A): hardlink unchanged files from a read-only base cache, break the link on write (the agent's tools write via temp + rename, which already breaks hardlinks; Bash `>>` would not, so detect and fall back). | fork time on 20k files < 150 ms; correctness test for in-place appends |
| M3.4 | **Per-call cgroups + fanotify** (where available) so background writers stay attributed to the call that started them. | T2.6 |
| M3.5 | **Warm build caches for search forks**: a shared read-only `target/` / `node_modules/` lower layer or `sccache`. | Rust fixture: second attempt's build time < 30% of the first |

## M4 — Smarter control

| # | Deliverable | Accepted by |
|---|---|---|
| M4.1 | **Model-proposed hypotheses** for search: one call asks for N distinct approaches (JSON), shown and editable before the fork. | T6.3 diversity improves over generic prompts |
| M4.2 | **Evaluator**: a separate cheap model call (or rules first) reads the record at stable points and decides: continue, rewind to X with lesson Y, or ask the human. Replaces "the failing agent decides". | T5 scenarios, without prompting the agent to rewind |
| M4.3 | **Record triggers**: same test red N times, identical error 3 times, 25 reads without a write → evaluator wakes. | unit tests on synthetic records |
| M4.4 | **Lesson library**: lessons per repo with the A/B effect measured by M2; injected only when the same failure signature recurs. | M2.5(b) shows no harm from injected lessons |
| M4.5 | **Cycle detection**: an attempt reaching a tree already seen in an abandoned branch gets a note. | unit test |
| M4.6 | **Per-hunk parking**: split a conflicted file's segment into hunks; apply clean hunks, park only conflicting ones. | T3.3 |
| M4.7 | **Flaky-check handling**: re-run a red check once before counting it; mark tests that flip as flaky and exclude them from rollback decisions. | T4.4 |

## M5 — Gym and forge

| # | Deliverable | Accepted by |
|---|---|---|
| M5.1 | **Forge API**: a script of intended tool calls executed against a snapshot; every result is computed; output is a branch with `origin: forged` events; replay check byte-compares. | forge → verify → replay equality |
| M5.2 | **Reverse curriculum**: from an eval task with its real fix, forge prefixes that are k% along the real fix path; difficulty dial. | generates valid starting states that pass `verify` |
| M5.3 | **Counterfactual credit**: V(k) = pass rate of N forks from turn k; binary search for the turn where V drops; report it on failed runs. | finds the planted bad turn in a scripted run |
| M5.4 | **Cassettes**: record/replay for HTTP the agent makes in a dry world. | replay test |
| M5.5 | **Labels from observation**: dry-world runs log what actually connected; feed the classifier. | T7.4 accuracy improves |

## M6 — Surface

| # | Deliverable | Accepted by |
|---|---|---|
| M6.1 | **Daemon**: `runestone` serving JSON-RPC over a unix socket + an event stream (subscribe to a branch). CLI becomes a client. | protocol tests |
| M6.2 | **TUI** on the event stream: conversation, live tool calls, checks, a branch tree; rewind/fork from the tree. | manual |
| M6.3 | **ACP server**: rune as an agent for editors and the voice agent. | ACP client test |
| M6.4 | **Companion MCP**: world tools (log, blame, plan/apply rewind, fork, search) for a supervising agent; never exposed to the worker. | MCP test |
| M6.5 | **Donor executors over ACP** (Kiro first), marked as an observational data tier. | T10-style run |

## M7 — Scale

Postgres backend with the same schema and a pull-queue for workers; Firecracker tier for forks that need running
processes; object store for trees. Only when M2 shows search or gym runs are worth scaling.

---

## What I do next, without waiting

Done: M1.1 Ctrl-C, M1.2 incremental fold, M1.4 gateway quirks, M1.6 per-worktree lock, M1.7 symlink-safe
writes, M1.8 `rune doctor`, M2.1–M2.4 `rune eval` (mine with `--validate`, run, report), M3.1 in-process
scan world (17–20 ms warm on 20k files).

1. **Shared build cache for eval cells** (each Rust cell compiles from scratch, ~35 s), then a first real
   `rune eval run` with Muse over rune's own validated tasks: plain vs checks vs rollback vs search.
2. **M2.5** experiments as eval variants.
3. **M3.3** faster copy forks (the base materialization now dominates a search's start).

I push each item to `main` with its tests and a line in TODO.md; anything that changes behaviour you test gets a
note in TESTING.md.
