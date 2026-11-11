# rune

Versioned worlds for coding agents. World and conversation share one state id: an event in a
content-addressed DAG. Every event snapshots the worktree, is attributed by construction, and can be
rewound to or forked from. The agent loop is a swappable part; `runestone` never talks to a model.

```
crates/runestone   engine (library): store, world, attribution, rewind, fork/bisect, classifier, hooks
crates/rune        CLI
```

## What works (v1)

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

## Use

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

## Tests

`cargo test` covers attribution, keep-human rewind, dependent-hunk parking, hard rewind + external guard,
plan/apply race, fork, bisect with cache, id verification, the classifier table and hook ingestion.
