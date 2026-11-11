# TODO

What is built, what is decided, and what is next, from the design discussions. Order within a section is
priority. Background: [versioned-worlds.md](versioned-worlds.md) (the original idea document) and the
design notes doc ("rune: design notes, deep pass").

## Decided

- Own harness in Rust, Linux first. Not a Pi Durable fork (its history is linear and single-writer).
- No Temporal or DBOS: the record is a content-addressed event DAG with CAS branch heads. SQLite now,
  the same schema on Postgres later.
- The conversation is a fold over the record; every request's bytes are stored and must re-render exactly.
- The agent sees human edits only between turns (per-turn isolation), merged at the turn boundary.
- Rewind keeps human work: try a three-way merge first, park a hunk as a patch only when it conflicts.
- Core first, UI later through an API. Donor ACP agents come after rune's own loop.

## Built (v1–v4)

- Event DAG, shadow-git world snapshots, attribution (agent / outside / unknown), blame, outside-changes note.
- Keep-human and hard rewind, undo, fork, turn bisect with a result cache, two-axis irreversibility classifier,
  Claude Code and Kiro hook ingestion.
- Own agent loop: Anthropic streaming provider, Read/Write/Edit/Bash/Grep/Glob, read-before-write pinning,
  self-rewind with a lesson, dry mode with offline sandbox, exact re-render (`rune verify`).
- Automatic checks at stable points, cached by tree, regression rollback to the best state, every tree pinned.
- Overlay (layered) worlds with O(changes) snapshots; `rune search` with hypothesis-forced parallel attempts,
  held-out tests, keep-human merge of the winner.
- TESTING.md, fixtures, per-turn token and cache-hit report.

## Next: make it real

- [ ] Run TESTING.md steps 0–6 against the real API; fix what breaks.
- [ ] Ask the model for N distinct hypotheses (one call) instead of requiring `--hyp`; check whether attempts
      actually differ.

## Measure (does it make the model better?)

- [ ] Eval from own repo history: revert real fix commits, keep their tests held out, run with and without
      rewind/search/checks, report a number.
- [ ] Restored world vs conversation-only rewind: score correct claims about the disk after a rewind.
- [ ] Does the model follow a planted wrong lesson? Decides how much the lesson channel needs protecting.
- [ ] Action-agreement metric for context building (same next tool + target with full vs compacted history).
- [ ] Shell classifier accuracy on a few hundred real shell calls before gating anything on it.

## Speed

- [x] Write git objects and trees in-process for layered worlds (own zlib + SHA-1 object writer, tree
      rebuild of changed directories only, in-process diff and gitignore). 0 git subprocesses per event;
      26 ms per step vs 101 ms on the scan world (20k files, 2 cores).
- [ ] Same for the scan world (`git add -A` + `write-tree` still spawn per event), e.g. stat-cache + own index.
- [ ] Base materialization dominates search start (0.6–5 s for 20k files, I/O-bound and noisy): reuse bases
      across searches, or populate lazily.
- [ ] Incremental fold: cache messages per head, fold only new events (today O(events) per step).
- [ ] Interactive sessions on overlay too (agent layer + merge at turn end), not only search forks.
- [ ] Warm build caches for search forks (shared, read-only `target/` or sccache).

## Correctness and isolation

- [x] Fork Bash commands run in their own mount namespace with the fork bind-mounted over the main worktree
      path, so indirect paths (env vars, paths read from files) land in the fork. Tested.
- [ ] File tools (Read/Write/Edit) still map paths by prefix; a symlink in the tree pointing at the absolute main
      path would be followed to the live tree.
- [ ] Per-hunk parking instead of whole-file parking on conflict.
- [ ] Rewind plan reports excluded paths that changed (cannot be restored).
- [ ] Two sessions in one worktree share the shadow index: lock or separate it.
- [ ] Per-call cgroups + fanotify so background writers (`npm run dev &`) stay attributed.
- [ ] Mechanical changes (formatters) detected by re-running the tool; re-derived on rewind instead of merged.
- [ ] Ctrl-C interrupts a turn cleanly.
- [ ] Optimistic-lock guard for the hook path (Claude Code / Kiro), like rune's own read-before-write.

## Features from the design

- [ ] Record-built compaction: deterministic skeleton (task, files by author, live external effects, last
      failure) + model-written narrative; old tool results elided to blob references with `read_at`.
- [ ] Self-rewind triggers from the record (same test red N times, repeated error) and a separate evaluator
      that decides when and where to rewind.
- [ ] Lesson library per repo, kept only with a measured A/B effect.
- [ ] Cycle detection: a branch reaching a tree hash already seen in an abandoned branch.
- [ ] Forge: intent forged, evidence computed by real execution, replay check, `origin: forged` only in forks;
      log-probability drift score for forged turns.
- [ ] Gym: reverse curriculum from real fixes, counterfactual credit (V(k) from N forks per turn, binary search),
      cassettes for external services, labels from observed effects in a dry world.
- [ ] Grounded work journal with a claim checker; context provenance manifest; evidence-backed completion.
- [ ] Firecracker tier for forks that need running processes; Postgres backend and pull-queue workers.

## Surface

- [ ] JSON-RPC daemon (`runestone`) with an event stream; TUI and richer UIs as clients.
- [ ] rune as an ACP agent; MCP for a companion agent that supervises (world tools never given to the worker).
- [ ] Donor executors over ACP (Kiro first), marked as an observational data tier.
