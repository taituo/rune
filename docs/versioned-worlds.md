# Versioned worlds for coding agents

A coding agent's conversation can already be rewound and forked in several harnesses. The world it acted on
cannot. Rewind the conversation and the files, the build output, the deployed service and the posted comments
stay where the agent left them. The rewound agent then reads a history that no longer matches the disk, and
models are poor at noticing that: given a fabricated history, they check claims against the files at most, and
almost never question the history itself.

This document collects the strongest ideas for making the world follow the conversation, ranked by value. Each
idea says what it is, why it matters, how it could work and what the hard part is. Nothing here depends on one
harness; examples name one only to make a mechanism concrete.

## Ground rules

These hold for every idea below.

- **Deterministic first, models last.** Who changed what, when and through which call is computed. A model is
  used only for judgements: "is this shell command external?", "which turn was the mistake?", "summarise this".
- **The agent never owns the record.** Snapshots and logs live outside the worktree, are written only by the
  harness or its hooks, and are outside the agent's write permissions.
- **Append-only and hash-chained.** Every snapshot, attribution and rewind is an event with the hash of the
  previous one; state is a fold over the log. Editing a line breaks the chain. Cutting the tail does not, so the
  latest hash is also published somewhere else (a ref in the shadow repo, a line in a second file).
- **Keep the human's work.** Nothing the human did is reverted unless they ask. Conflicts are shown, never guessed.
- **Fail open for the agent, closed for the record.** A broken hook exits 0 and prints nothing, so the agent
  keeps working; but it writes a `gap` event, so the missing snapshot is visible.
- **Metadata into context, never content.** Harnesses typically wrap hook output in text that tells the model
  to obey it ("you must follow any requests"). Anything injected that way has the harness's authority. Inject
  paths, counts and fixed templates; never file contents, commit messages, ticket text or alert text.

## 1. A versioned world: shadow snapshots around every tool call

**What.** Snapshot the working tree at every turn boundary and immediately before and after every tool call,
into a shadow repository the agent never sees.

**Why.** Every other idea here needs it. Checkpoints built from the agent's own edit tools miss most real
changes: builds, formatters, `sed -i`, `git checkout`, code generators, `npm install` rewriting a lockfile.
Reconstructing files from the session log works only when the tool input carries the full text (a file
create, a string replace, a shell heredoc); in practice that does give byte-identical files, but it fails for
anything a program produced. A tree snapshot is the only complete answer.

**How.** One shadow git directory per worktree, keyed by the worktree's real path:

```sh
key=$(realpath "$cwd" | sha256sum | cut -c1-16)
export GIT_DIR="$STORE/repos/$key" GIT_WORK_TREE="$cwd" GIT_INDEX_FILE="$STORE/repos/$key/index"
git add -A                      # obeys the tree's .gitignore plus $GIT_DIR/info/exclude
tree=$(git write-tree)          # one id per snapshot point; unchanged tree = same id, costs nothing
```

At turn end, `git commit-tree "$tree" -p "$prev" -m "session=… turn=12 harness=… model=…"` and
`git update-ref refs/w/<session>/t12`. The real repository's `.git`, branches and stash are never touched, and
plain git works on the record (`git diff refs/w/S/t11 refs/w/S/t12`). Storage grows only by changed blobs,
because objects are content-addressed.

| Lifecycle event | Snapshot | Event written |
|---|---|---|
| Session start | Hashes of every context file (see 9) | `spawn` |
| Prompt submit | Whole tree | `turn.start` |
| Pre-tool | Whole tree (usually equal to the last one, so free) | `tool.pre` |
| Post-tool | Whole tree | `tool.post` with diff stat |
| Stop | Whole tree, as a commit | `turn.end` |

In Kiro CLI these are agentSpawn, userPromptSubmit, preToolUse, postToolUse and stop hooks; in Claude Code
SessionStart, UserPromptSubmit, PreToolUse, PostToolUse and Stop. Each hook gets a JSON document on stdin with
the session id, cwd, tool name and tool input.

One event line:

```json
{"v":1,"ts":1794385056.783,"prev":"9f2c…","harness":"kiro","session":"7a37…","turn":12,
 "kind":"tool.post","call":"toolu_01…","tool":"shell","label":"local",
 "tree_before":"4b1d…","tree_after":"c07e…","changed":[["M","src/x.go",12,3],["A","gen/api.pb.go",880,0]]}
```

The exclude list is strict by default: `.env*`, `*.pem`, `id_*`, `*.key`, `*.sqlite3`, `node_modules/`,
`target/`, `dist/`, `build/`, `.venv/`, anything over 5 MB. An excluded path that changed is still recorded
(`["M","db.sqlite3","excluded"]`), so a rewind knows what it cannot restore.

**Hard part.** Speed and secrets. `git add -A` on a large monorepo can take seconds, and hooks typically have a
timeout around 10 s and run synchronously. Levers, in order: the untracked cache and a filesystem monitor
(`core.untrackedCache`, `core.fsmonitor`); snapshot only the paths a call could touch (a file edit names its
path; a shell call does not, so it gets the full scan); and finally turn boundaries only, which still gives
human-vs-agent attribution but not per-call. The store copies whatever is in the tree, so it is a secret store
by accident: mode 700, never sent to a model, and a `purge PATH` command that rewrites it.

## 2. Deterministic attribution: agent, human or unknown

**What.** Every change in the tree is assigned to its author from the snapshots alone.

| Change appears between | Attributed to |
|---|---|
| pre-tool and post-tool of call C | The agent, call C (tool and input) |
| end of turn N and start of turn N+1 | Outside: the human, an editor, a formatter, another agent, a `git pull` |
| inside a turn but outside any call | Unknown: a concurrent writer; flagged |

A worked timeline:

| Point | Tree | Diff from previous | Author |
|---|---|---|---|
| t11.end | `a1` | | |
| t12.start | `b2` | `M README.md` | outside (between turns) |
| pre call 1 (shell `go generate`) | `b2` | | |
| post call 1 | `c3` | `A gen/api.pb.go`, `M go.sum` | agent, call 1 |
| pre call 2 (edit `src/x.go`) | `d4` | `M src/x.go` | unknown (inside the turn, no call running) |
| post call 2 | `e5` | `M src/x.go` | agent, call 2 |

The "unknown" row is typical: the human saved the same file in an editor while the agent was thinking.

**Why.** It turns "the agent did something to my repo" into a per-file, per-call audit trail that ends at the
prompt which caused it. It is the input to notices (3), rewinds (4), the journal (8) and blame:

```
src/x.go
  t12 call 2  agent  edit   "fix the nil check in the handler"   +4 -1
  t12 (gap)   unknown       +1 -0
  t9  call 5  agent  shell  "gofmt -w ."                         +0 -2
  t8→t9       outside        +12 -3
```

No model decides authorship; a model only phrases a change ("the user renamed `foo` to `bar` in three files").

**How.** Diff consecutive trees. Narrow "outside" and "unknown" by elimination: other live sessions in the same
worktree claim their own `tool.post` diffs first; the real repository's reflog explains checkouts, pulls and
rebases; an optional file watcher (inotify, fanotify) records the writing process for out-of-band changes.
Whatever is left between turns is a human edit.

**Hard part.** Concurrent writers and background processes. Two agents in one worktree make attribution
"unknown" by construction; the answer is one worktree per agent, with cross-session claims for when that rule
is broken. A shell call that starts a watcher or dev server (`npm run dev &`) keeps writing after its post-tool
snapshot; without process tracking those writes land in later calls or in "unknown". Session logs usually have
no timestamp per tool call, so hook time is the only real time.

## 3. Telling the agent what the human changed

**What.** At the start of each turn, inject a short note listing outside changes since the agent's last turn.

```
Since your last turn (turn 41), outside changes in this worktree:
  M src/handler.go (+12 -3)   M README.md (+1 -1)   D tmp/test.out
Not made by you. Read them before editing these files.
```

After a resume or rewind the template changes:

```
This session was resumed after 3 days. Outside changes since turn 41: 27 files (14 under src/, 9 under docs/,
4 others; first 10 listed below). The repository's HEAD moved from 4f1a… to 9c2e… (2 pulls).
```

**Why.** Agents overwrite human edits because they edit from a stale copy of the file in their context. They do
not go looking for changes on their own, and they do not read their own session logs; the note has to be
pushed. Stored in the session as hook output, it also becomes part of the record.

**How.** A prompt-submit hook computes the outside diff (from 2) and prints the fixed template: at most 10
paths, then counts per directory, never content. It costs tokens on every turn in which something changed and
stays in history, so it must stay short.

An optional pre-tool guard gives optimistic locking. On every successful read, record the blob id the agent saw
(`last_read[path] = blob`). Before a write, compare the file's current blob with it:

| Current file | last_read | Decision |
|---|---|---|
| Does not exist | none | Allow (new file) |
| Equal to last_read | any | Allow |
| Changed by this agent since the read | | Allow (its own edit) |
| Changed by someone else since the read | | Block, exit 2: "src/handler.go changed outside since you read it (+12 -3). Read it again before editing." |
| Exists, never read | none | Warn once, then allow |

In Kiro CLI and Claude Code a pre-tool hook that exits 2 blocks the call and its stderr reaches the model.

**Hard part.** The channel's authority: injected text is obeyed, so it must stay metadata. Compaction later
summarises old notes away, which is one more reason the record lives outside the conversation. And the guard
only sees edit tools; a `sed -i` through the shell passes it, and is caught only after the fact by 2.

## 4. World rewind that keeps human work

**What.** Fork the conversation at turn N and bring the files back to turn N, reverting only the agent's changes
after N.

**Why.** It removes the mismatch between memory and disk that makes a rewound agent unreliable, without
destroying what the human did meanwhile.

**How.** Three modes:

| Mode | Files | Use |
|---|---|---|
| keep-human (default) | Revert each agent call after N, newest first, by three-way merge | Normal rewind |
| hard | Check out the tree at the end of turn N; save the current tree as a shadow commit first | Start clean |
| none | Conversation only; the agent is told the world is ahead of its memory | Today's behaviour |

The keep-human revert is `git revert` per call, applied to the working tree:

```
for call in reversed(agent_calls_after(N)):
    for path in call.changed:
        result = merge3(base=after[path], ours=current[path], theirs=before[path])
        # base→theirs is "undo the agent"; base→ours is "everything since, incl. the human"
        if result.conflict: record_conflict(path, call, result.hunks)  # data, not markers
        else: write(path, result.text)
```

Example. Turn 18 the agent edits lines 10–20 of `handler.go`. Between turns the human edits line 50. Turn 20 the
agent edits line 15 again. Rewind to 17: both agent edits are reverted, the human's line 50 stays. Had the human
edited line 15 after turn 18, that hunk becomes a conflict:

```json
{"path":"src/handler.go","call":"t18/c3","hunk":[13,17],
 "agent_before":"if err != nil {","agent_after":"if err != nil && !retry {","now":"if err != nil && !retry && ok {"}
```

Files are handled by kind: an agent-created file is deleted only if unchanged since; an agent-deleted file is
restored from `before`; binary files never merge, they are restored if untouched and reported otherwise.

Every rewind starts as a plan, and a dry run prints it:

```
rewind to turn 17 (keep-human)
  revert  11 agent calls in 7 files     conflicts: 1 (src/handler.go:13-17)
  keep    3 outside changes (README.md, docs/api.md, src/handler.go:50)
  cannot  db.sqlite3 (excluded, changed in t19)
  outside world, still live after rewind:
          t19 call 4  external  git push origin feature/retry
          t21 call 2  external  ticket comment on PROJ-123
  needs   --accept-external
```

The rewind is itself an event that saves the pre-rewind tree, so an undo restores both world and conversation.
The new session's first note says what was restored, kept and not restorable.

**Hard part.** It is a destructive operation on a real worktree. A first version refuses when excluded or
oversized paths changed after N, always shows the plan first, never touches the real repository's metadata, and
never crosses an irreversible action silently (5). Sessions recorded before snapshots existed can be rewound
only partly, by reversing self-contained edit calls from the log; shell changes are reported as not reversible.
Conflicts need a resolution UI, or a separate agent whose resolution is evaluated like any other change.

## 5. Irreversibility classes for actions

**What.** Label every tool call by what a rewind can undo.

| Class | Meaning | Examples |
|---|---|---|
| read | No side effect | read, grep, `git status`, `git diff`, `kubectl get`, ticket search |
| local | Changes only files in the snapshotted tree | edits, `sed -i`, `go build`, `git commit` |
| local-outside | Changes local state outside the tree | `pip install --user`, `~/.config` edits, `/tmp`, local container images |
| external | Changes something nobody can restore | `git push`, deploys, `kubectl apply`, `terraform apply`, ticket and review writes, email, chat messages |

**Why.** The outside world cannot be rewound. A rewind past a deploy or a posted comment must say so and list
what is still live. The same labels give cheap control:

- **Dry world.** An unattended agent's external calls are blocked at pre-tool and queued as proposals:
  "Blocked: `git push origin feature/retry` is external. Queued as proposal #3 for a human."
- **Effect budget.** "At most one deploy and three ticket comments per session", enforced at pre-tool.
- **Effect ledger.** "What did this agent do to the outside world this week?" is one query over the log.

**How.** Three tiers, cheapest first, and the first one decides:

1. **Static rules.** Tool type (read tools are read); integrations by operation name (`add_comment`,
   `transition_issue`, `create_issue` are external; `search_*`, `get_*` are read); shell rules on the parsed
   command, not the string:

   | Shell pattern | Class |
   |---|---|
   | `git push`, `git push --force` | external |
   | `git push --dry-run`, `kubectl … --dry-run=client`, `terraform plan` | read |
   | `curl` / `wget` with `-X POST\|PUT\|DELETE`, `-d`, `--data*` | external |
   | `curl` GET to a host | read (but data may leave; see the hard part) |
   | `ssh host CMD` | external unless CMD is known read and the host is allowlisted |
   | `make X`, `./script.sh` | unknown → tier 2/3 |

   A compound command (`cd a && make && git push`, pipes, `;`, `$(…)`) takes the highest class of its parts.

2. **Observed effect.** After the call, the snapshots show whether the tree changed. Inside a sandbox with its
   own network namespace, connection accounting shows whether anything left the machine.
3. **Model.** For commands still unknown: a small model with the command, the cwd, the script's text if it is a
   local file, and the rules as few-shot examples. Stored with its confidence and model id. A human override
   becomes a new static rule.

Unknown is treated as external for gating and reported as unclassified, so a gap in the rules fails safe.

**Hard part.** Shell commands are open-ended: `make deploy`, a Python script calling an API, a test suite that
posts to a shared staging service. And "read" is not harmless in every sense: a GET can exfiltrate data in its
URL. The classifier answers "can a rewind undo this?", not "is this safe?". Its accuracy has to be measured on
a few hundred real shell calls before anything is gated on it.

## 6. Harness-written sessions and harness-owned compaction

**What.** The harness, or a tool beside it, does not only read and cut session logs; it writes new ones.

**Why.** Compaction by summary loses detail, and loses more each round, because each summary is made from the
previous summary plus the new messages. Measured on one harness, with facts planted in the conversation:

- 7 of 7 facts survived the first compaction, 2 the second, 1 the third.
- A fact the user marked "Important: remember X" was dropped in the very first round, because the same message
  also held 35 000 characters of filler.
- After compaction the model answered "no" about a fact that was in its own summary.

Not all context is equally fragile. What the harness rebuilds from disk on every request (system prompt, rules
files, skill descriptions, session-start hook output, the task list) survives compaction untouched; only the
history (messages, tool results, per-prompt hook output) is summarised. A session written from the record can
put the deterministic facts in the durable places and verbatim.

**How.** Uses, best first:

- **Consistent history after a rewind.** Instead of a fork plus a note, write a fork whose last event is a
  clearly marked harness event:

  ```
  [harness event, not from the user] World restored to turn 17 (keep-human).
  Kept outside changes: README.md, docs/api.md. Not restorable: db.sqlite3.
  Still live in the outside world: branch feature/retry pushed (t19), comment on PROJ-123 (t21).
  ```

- **Own compaction.** A fresh session built from the record, with fixed parts and budgets:

  | Part | Source | Budget |
  |---|---|---|
  | Task | First prompt verbatim + the latest restatement of the goal | 1 500 chars |
  | Files changed, by whom | Attribution (2), deterministic | 40 lines |
  | Still live outside | Effect ledger (5), deterministic | all |
  | Open problems | Last failing test output, last error, verbatim and trimmed | 2 000 chars |
  | Narrative | Model-written from the journal (8) | 400 words |
  | Last turns | The last 3 turns verbatim | rest of budget |

  Prior art: at least one harness already appends a deterministic "factual record" (files read and edited) to
  its model-written summary. This goes further: the deterministic part is the skeleton, the model fills gaps.
- **Handoff sessions.** A new agent, in any harness, starts from a session that contains exactly what it needs,
  instead of a handoff file someone has to remember to mention.
- **Replay fixtures.** A real session cut at turn N, plus the world snapshot of turn N, is a regression test:
  re-run turn N+1 on another model or prompt and compare the diff, the tests and the cost.

**Hard part.** Trust and honesty. Models cannot reliably tell a forged history from a real one, which is exactly
why this is powerful and dangerous. Synthetic content is labelled as coming from the harness, never puts words
in the human's mouth, is written only into forks, and the record says what was written, from what and by whom.
Session formats change between harness versions, so writers need tests against recorded fixtures; and
harness metadata such as "why was this session created" is often too coarse to tell a written session from a
real one, so provenance must come from the record (9).

## 7. Self-rewind with a lesson: accepting amnesia

**What.** An agent that judges its own path wrong rewinds itself, conversation and world, to an earlier turn and
continues with one lesson. It forgets everything after that turn.

**Why.** Long failing paths fill the context with noise, compaction then mixes noise and signal, and the agent
spirals: more attempts, worse summaries, more attempts. A clean restart with one sentence is cheaper and more
reliable than pushing on. Accepting amnesia is the point.

**How.** A tool the agent may call:

```
rewind_self(to_turn=14, lesson="Patching the retry in the client failed: the timeout comes from the proxy
(see 502 in proxy log). Start from the proxy config.")
```

It runs a keep-human world rewind (4) and starts a session whose first note is only the lesson. The lesson must
name the approach that failed and the evidence, in at most 300 characters; a lesson without evidence is
rejected.

Triggers come from the record, not from the agent's mood:

| Signal | Example threshold |
|---|---|
| Tests red for many turns | Same test failing 4 turns in a row |
| Repeated error | Identical error text 3 times |
| Searching without acting | 25+ reads/searches in a row with no write or shell |
| Context pressure | Second compaction within one task |
| Reviewer verdict | An evaluator agent returns NEEDS_CHANGES twice for the same reason |

Better still, a separate evaluator decides when to rewind and to where, not the agent that failed. The rewind
point is usually the last turn where tests were green or the plan was stated, both findable in the record.

With world rewind, branching is cheap: fork turn 14 into two worktrees, give each a different approach, keep the
one whose tests pass, keep the other as an abandoned branch. That is tree search over real worlds. The same
mechanism compares two models or two harnesses on identical work. When the winning branches are merged, the
merged tree is what gets tested: two branches can each be green and break together.

**Hard part.** Guards and accountability. At most 3 self-rewinds per task and never past an external action
without a human; every self-rewind is an event with its lesson; abandoned branches stay readable. Open research
questions: does a model follow a lesson from "itself" blindly, even a forged one? Does it behave differently
when it knows it is a rewound copy? Is it still accountable? The record ties every change to the branch that
produced it, not to the agent's name.

## 8. A grounded work journal

**What.** Per-turn summaries, rolled up per session, day and week, that answer "what have I done" in the shape
the user wants, with every sentence citing its source.

**Why.** It is memory that survives compaction because it lives outside the conversation, it is the content of
handoffs and own compaction (6), and it is what most users actually want from session history.

**How.** After each turn, a cheap model writes one to three lines from the prompt, the final answer, the
attributed changes (2) and the external effects (5), stored in a full-text index (`find summary:deploy`). The
user gives two or three examples of the output they want; they become few-shot examples. A weekly report might
come out as:

```
Mon  Fixed the nil check in the payment handler; tests green.        [s7a37 t12-15]
     Pushed feature/retry for review.                                 [s7a37 t19, external]
Tue  Investigated proxy timeouts; root cause in proxy keep-alive.     [s91c0 t3-22]
     Abandoned client-side retry approach (self-rewind to t14).       [s91c0 t14]
```

A checker runs before anything is shown:

| Claim in a sentence | Must be true in the record |
|---|---|
| Cited turn exists | Session and turn in the index |
| "Changed / fixed X" | X is in the attributed changes of the cited turns, authored by the agent |
| "Tests green / pass" | A test command in the cited turns with exit status 0, after the last change |
| "Pushed / deployed / commented" | An external event in the cited turns |

A sentence that fails is removed, not reworded. Recurring work appears by clustering prompts across sessions
("deploying to the dev cluster came up in 9 sessions over 3 weeks"), which shows what to automate.

**Hard part.** A summary can look right and be wrong, and a fluent journal is trusted. The checker is what makes
it usable. Access is a privacy question: an agent that can read every session sees everything done on the
machine, so journal access is opt-in per agent and defaults to the current worktree.

## 9. The record as evidence

Smaller ideas that come almost free once 1, 2 and 5 exist.

**Context provenance.** At session start, record by hash exactly what the model was given: system prompt, rules
files, skill descriptions as rendered, hook output, model id, and the share of the context window they use.

```json
{"kind":"spawn","agent":"reviewer","model":"…","window_used_pct":40.1,
 "context":[["prompt","sha256:…",1200],["rules/style.md","sha256:…",8400],["resource:projects.yml","sha256:…",281000],
            ["missing","prompts/reviewer.md"]]}
```

Harnesses rebuild this on every request and store none of it, and the failures are silent. Seen in practice: an
agent whose prompt file was missing ran with no prompt and no warning; skill descriptions written as multi-line
YAML reached the model as the literal text `>-`, so it saw only names; one agent spent 40 % of its window on a
single resource file before the first message and compacted twice as early as its peers. A manifest makes each
of these a one-line diff.

**Evidence-backed completion.** "Tests pass", "built", "deployed" in a final answer are checked against the
recorded tool results of that turn, not the agent's text, with the same rules as the journal checker.

**Observed dependencies.** When an agent reads another agent's unfinished work, the read is logged. If that work
is later abandoned or rejected, everything built on it is flagged, even after it was merged.

**Protected paths.** An implementer agent cannot modify the evaluator's tests or a held-out test set; only a
separate, privileged task can, and the attempt is an event.

## A worked scenario

1. Turn 12: the user asks to fix flaky retries. The agent edits `client.go`, runs tests (red), edits again.
   Every call is snapshotted and attributed to the agent.
2. Between turns 15 and 16 the user fixes a typo in `client.go` and updates `README.md`. Turn 16 starts with a
   note listing both files; the agent re-reads `client.go` before editing it.
3. Turns 16–20: the agent pushes a branch (external, logged) and keeps failing on the same timeout. Same error
   three times triggers the evaluator, which proposes rewinding to turn 14 with the lesson "the timeout is in
   the proxy".
4. The plan shows: revert 9 agent calls, keep the user's 2 changes, the pushed branch stays live. The user
   accepts. The world is back at turn 14 plus the user's edits; the new session's first note is the lesson and
   the live branch.
5. Turn 15': the agent goes to the proxy config, tests go green. The journal for the day reads "fixed proxy
   keep-alive; abandoned client-side retry", each line cited, the push listed as still live.

## Harness strategy

The core (store, log, attribution, rewind, classifier, journal) knows nothing about any one harness. Each
harness gets an adapter:

```python
class Adapter(Protocol):
    def events(self, raw_hook_input: dict) -> Event: ...        # hook JSON → spawn/turn/tool events
    def read_session(self, session_id: str) -> list[Turn]: ...  # parse the session log
    def fork(self, session_id: str, at_turn: int, extra: list[HarnessEvent]) -> str: ...
    def inject(self, note: str) -> HookOutput: ...              # how text reaches context
    def install(self, dry_run: bool) -> Plan: ...               # hooks in/out, idempotent
```

Install edits configuration that humans and other tools also edit, so every added hook carries an ownership
marker (`"description": "world:snapshot-v3"`); install adds missing and upgrades older marked hooks, uninstall
removes exactly the marked ones, writes are atomic, the original is backed up, and a dry run prints the plan per
file. Configuration managed by a sync tool will silently lose the hooks on the next sync, so `check` must detect
that.

| Route | What it is | Gains | Costs |
|---|---|---|---|
| A. Hooks | Recorder and notes as hook scripts in existing harnesses | Works today, small adapters | Hooks may be per agent (a built-in default agent may have none), timeouts, no view of what the harness injects, cannot change compaction or rewind |
| B. Wrapper | A process that starts the harness and owns its environment: file watcher, PATH shims, network proxy, sandbox | Sees shell writes and network for every agent, no config edits, any harness | Sees the conversation only through logs; shims are fragile |
| C. Model-API proxy | Sit between the harness and the model API, or speak its agent protocol (ACP in some harnesses) | Sees the exact context sent to the model; can inject and redact | Format coupling, credentials in the middle, high security bar |
| D. Fork an open-source harness | Build the core into an existing open-source coding agent | World-aware rewind and own compaction as first-class features | Maintaining a fork; losing the original harness's features and models |
| E. Own harness | A small agent loop designed around the record | Versioned worlds from the start; agents disposable, the log is the truth | Biggest build; competing on UX and tools |

Recommended: A and B now, C as a research probe inside a sandbox with a throwaway account, D or E only when world
rewind and self-rewind have proven their value and hooks are the bottleneck.

Prior art worth borrowing from: some harnesses already keep a shadow git repository for undo, which validates
idea 1; checkpoints limited to the harness's own edit tools show the cost of ignoring shell changes; auto-commit
to the real repository is simple but pollutes history and fights the human. The new parts here are snapshots
around shell calls, attribution, keeping human work on rewind, irreversibility classes, and one timeline per
worktree across harnesses, so a turn in one harness, a human edit and a turn in another land in one record.

## Experiments before building

| Question | Method | Decides |
|---|---|---|
| How long does a snapshot take on a large repository? | `add -A` + `write-tree` with a shadow index, cold and warm, with and without untracked cache and fsmonitor; p50/p95 per call | Per-call snapshots, or turn boundaries only |
| What happens when a hook times out or fails? | Hooks that sleep past the timeout, exit 1, print garbage | Whether "fail open" holds |
| Does a restored world help a rewound agent? | Same fork and probe twice: world=none vs world=keep-human; score correct claims about the disk | Whether world rewind is worth its risk |
| Does a model follow a forged "lesson from yourself"? | Self-rewind sessions with a planted wrong lesson | How much the lesson channel must be protected |
| How accurate is the shell classifier? | Label a few hundred real shell calls by rules, hand-check a sample, model on the rest | Whether classes can gate anything |
| Does record-built compaction keep more? | Plant facts, compact with the harness and with the record, count survivors over 3 rounds | Whether to own compaction |