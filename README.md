# rune

A coding agent whose conversation and world are versioned together. Both share one state id: an event in a
content-addressed DAG. Every event snapshots the worktree and is attributed by construction. The
conversation is a fold over the same events, so a rewind or fork moves the messages and the files at once.

## Agent loop

```sh
export ANTHROPIC_API_KEY=…
rune chat                         # new session (default mode: ask before outside-world actions)
rune chat -r                      # resume the latest session in this worktree
rune run "fix the failing test"   # one request, then exit (default mode: dry)
rune transcript                   # the conversation exactly as the model will see it next
rune verify                       # re-render every sent request from the record, compare bytes
```

In chat: `/rewind N|green [hard]`, `/undo`, `/fork N DIR`, `/check`, `/log`, `/continue`, `/verify`.

Checks: `--check CMD` (default: detected from Cargo.toml, go.mod, package.json, pytest, Makefile),
`--check-every N` (also mid-turn after N file-changing calls), `--rollback-after N` (restore the best
checked state after N consecutive worse results). `rune check` runs it by hand; `rune rewind --green`
targets the last passing check.

| Piece | How |
|---|---|
| Conversation = fold | Messages are rebuilt from the effective chain on every step. A `world.rewind` jumps to its target and appends a note, so history stays append-only (prompt cache and thinking signatures stay valid). There is no separate message store. |
| Exact data | The rendered request bytes of every model call are stored (content-addressed). `rune verify` re-renders them from the record; a mismatch means the record no longer explains what the model saw. The system prompt and tool schemas are stored once at spawn and reused byte for byte. |
| Tools | Read, Write, Edit, Bash, Grep, Glob (the shapes current models are trained on), plus Rewind. |
| Read-before-write | Every Read and Write pins the content hash the agent saw. Edit/Write refuse a file not read, or changed since. Rebuilt from the record on resume. |
| Self-rewind | `Rewind{point, lesson ≤300 chars, evidence}` restores conversation and files (keep-human) to the start of the current request or before a given call, then continues with only the lesson (plus the request, repeated). Max 3 per request; never past an outside-world action without a human. |
| Dry world | `--mode dry`: known outside-world actions (push, deploy, HTTP writes, MCP writes) are blocked; commands rune cannot classify run in an unprivileged user+network namespace, so they change the workspace but cannot reach outside. `--mode ask` asks; `--mode auto` runs everything. |
| Bash | Own process group, output to a file (background children don't block), timeout kills the group, head+tail clipping. |
| Checks | At stable points (turn end, optionally every N changing calls) the recorded world is built and tested in a long-lived per-worktree workspace moved by changed files only, so build caches stay warm; offline when possible. Results keyed by (tree, cmd): a world is never checked twice. Scores parsed from cargo, pytest, jest and go output, or `rune-score: N`. Failures and recoveries reach the model as harness notes. |
| Auto-rollback | After N consecutive checks worse than the best, a keep-human rewind to the best checked state, with the undone attempts listed for the model. At most 3 per request; never past an outside-world action. Abandoned branches stay in the record, and every recorded tree is pinned with a ref so git gc cannot collect them. |
| Model | Anthropic Messages API, SSE streaming accumulated into the non-streaming shape, retries on 429/5xx/529 and stream errors. `--model`, `--effort`, `--show-thinking`. |

## Versioned world

```
crates/runestone   engine (library): store, world, attribution, rewind, fork/bisect, classifier, hooks
crates/rune        CLI
```


| Piece | How |
|---|---|
| Event DAG | SQLite, id = sha256(parents, kind, ts, body), branch heads moved by compare-and-swap. Same schema for Postgres. `rune verify` recomputes every id. |
| World snapshots | Shadow bare git per worktree (`$RUNE_HOME/repos/<key>`), mode 700, strict default excludes (`.env*`, keys, `*.db`, `node_modules/`, `target/` …). Real `.git` never touched. |
| Attribution | From lifecycle position: `tool.pre→tool.post` = agent (that call), `turn.end→turn.start` = outside, anything else = unknown. |
| Outside-changes note | At `turn.start`: paths and counts only, never content. |
| Keep-human rewind | Rebase in plumbing: start at the target's tree, cherry-pick every non-agent segment after it (incl. unrecorded live edits) with `git merge-tree`, skip the agent's. Conflicts = human hunks that depended on agent work → parked as patches, never merged. Refuses to cross external actions without `--accept-external`, refuses if the worktree moved since the plan. Undo = restore the pre-rewind tree. |
| Hard rewind | Back to the target tree exactly. |
| Fork | Materialize any event's world into a new dir as a new branch sharing history (objects via git alternates). |
| Turn bisect | Binary search over distinct recorded worlds for the first where a command fails. Results cached by (tree, cmd). |
| Classifier | Two axes: state (read / local / local-outside / compensable / unknown / irreversible) × disclosure (none / internal / external). Static rules on parsed shell (compound, `$(…)`, redirects) and MCP op names. Unknown fails safe. |
| Hooks | `rune hook` ingests Claude Code and Kiro hook JSON. Fails open (writes a `gap` event). `RUNE_DRY_WORLD=1` blocks compensable/irreversible/unknown at pre-tool (exit 2). |
| Bench | `rune bench` → cold and warm p50/p95 snapshot latency. 20k files: ~360 ms cold, ~25–40 ms warm. |

## World commands

```sh
cargo build --release
rune install claude          # prints hook config to merge into .claude/settings.json
rune install kiro            # same for a Kiro agent config

rune log                     # events on the latest branch of this worktree
rune blame src/x.go          # per-path: turn, event, +/-, author, tool, call summary
rune changes                 # unrecorded changes since the last event
rune rewind --turn 14        # plan only
rune rewind --turn 14 --apply [--accept-external]
rune rewind --undo
rune fork --turn 14 ../try-a --name try-a
rune bisect --cmd 'cargo test -q'
rune classify -- 'cd a && make && git push'
rune bench
```

Without hooks you can drive it by hand: `rune record turn.start|tool.pre|tool.post|turn.end|manual`.

## Known limits

- Snapshots scan (`add -A`). Overlay write-capture slots in behind `World::snapshot()`.
- A conflicted file parks the whole segment's patch for that file, independent hunks included. Per-hunk split next.
- No mechanical-change detection, cgroups/fanotify, mtime handling beyond "restored files are written now".
- Excluded paths are never restored and not yet reported as "changed but excluded".
- No daemon/JSON-RPC yet; the library API is the API.
- No compaction yet (1M context); record-built compaction is next.
- No Ctrl-C interrupt inside a turn; killing the process is safe (resume closes dangling calls).

## Tests

`cargo test`: agent loop with a scripted model (exact re-render, append-only history, read guard,
self-rewind, dry world, offline sandbox, human rewind + continue), the HTTP/SSE provider against a mock
server (retry, thinking + signature, tool input JSON), and attribution, keep-human rewind, dependent-hunk parking, hard rewind + external guard,
plan/apply race, fork, bisect with cache, id verification, the classifier table and hook ingestion.
