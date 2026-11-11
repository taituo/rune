# Testing rune with a real model

Everything so far was verified with scripted agents and a mock API. This plan checks the parts only a
real model can answer, in order of risk. Each step says what to run, what should happen, and what to
note. Stop at the first step that fails and report it.

## 0. Setup (10 min)

Linux with Rust, git ≥ 2.38, python3 and util-linux.

```sh
git clone https://github.com/taituo/rune && cd rune
cargo build --release && cargo test          # 24 tests; namespace tests skip themselves if unsupported
unshare --user --map-root-user --mount true && echo namespaces-ok   # needed for search and offline mode
export PATH="$PWD/target/release:$PATH"
export ANTHROPIC_API_KEY=…                    # in your shell only, never in a chat
scripts/fixtures.sh ~/rune-fixtures           # calc, calc-hidden, trap
```

Optional: `export RUNE_HOME=~/rune-test-home` keeps the test record apart from real use.

**OpenAI-compatible gateway instead of Anthropic.** rune speaks chat/completions too:

```sh
export RUNE_API=openai RUNE_BASE_URL=http://10.91.1.1:8788    # with or without /v1
export OPENAI_API_KEY=…                                        # only if the gateway wants one
rune models                                                    # pick an id from this list
export RUNE_MODEL=<id>
```

The API is fixed per session (stored at its start), so `rune chat -r` keeps using the gateway.

**If `unshare … true` fails** (`write failed /proc/self/uid_map: Operation not permitted`), unprivileged user
namespaces are blocked: usually Ubuntu's AppArmor setting, or running inside a container. Then:

- steps 1–4 still work, but use `--mode ask` or `--mode auto`: without the offline sandbox, dry mode blocks every
  command rune cannot classify, including `python3 test_visible.py`;
- step 5 (search) needs namespaces. On Ubuntu: `sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0`
  (until reboot), or run in a VM.

## 1. Does it talk to the API at all? (2 min)

```sh
cd ~/rune-fixtures/calc
rune run --mode ask "List the files here and say in one line what calc.py does. Do not change anything."
rune verify
```

Expect: streamed text, a few Read/Glob calls, and a final line like `[s/… · 3 steps · end_turn · in … · cache read …]`.
`rune verify` should end with `N model requests re-rendered …, 0 differ`.

Note: any HTTP error (copy it whole), the step count, and the cache-read percentage (gateways that do not
report cached tokens show 0%).

## 2. A real fix with checks (5 min)

```sh
cd ~/rune-fixtures/calc && git checkout -q . && git clean -fdq
rune chat --check "python3 test_visible.py"
› the tests fail; fix it
```

Expect: the model reads calc.py, edits `a - b` to `a + b`, and runs the test. A dim `✓ check 2 passed, 0 failed`
appears at the turn end. Then in chat:

```
/verify
/log 15
```

Note: whether it read before editing (it must: Edit refuses otherwise), the cache-read % on the second
model call (should be well above 0: history is append-only), and anything it did that surprised you.

## 3. Your edits between turns, and rewind (10 min)

In the same chat:

1. Ask: `add a sub(a, b) function`.
2. Before the next prompt, edit `calc.py` yourself: add a comment line at the top.
3. Ask: `add a docstring to every function`.

Expect: the turn starts with a yellow `[harness event…] Since your last turn (turn 2), outside changes…: M calc.py`.
The model re-reads calc.py and keeps your comment.

4. `/rewind 2` and answer `y`.

Expect: the plan shows the docstring edits as reverted and your comment as kept. After applying, `calc.py` has
`sub` and your comment but no docstrings. Ask `what did we do so far?`: the answer should not mention docstrings.

5. `/undo`: the docstrings come back.

Note: did the model read your change before editing? Did its answer after the rewind contradict the files?
This is the key question from the design doc ("does a restored world help a rewound agent").

## 4. Regression rollback and self-rewind (15 min)

```sh
cd ~/rune-fixtures/trap
rune chat --check "python3 test_client.py" --check-every 1 --rollback-after 2
› the client times out; fix the client
```

The error blames `client.py`; the cause is `TIMEOUT_SECONDS = 0` in `config.py`. A model that patches the
client repeatedly makes the check worse or no better. Watch for one of:

- a red `✗ check`, then a yellow note `Automatic checks got worse 2 times…` and the files back at the best state;
- the model calling `Rewind` itself (shown as a tool call), with a lesson;
- the model going straight to config.py (fine, and worth noting: the trap was too easy).

Then `/log 30` and `rune transcript | tail -20`.

Note: which of the three happened, and whether after a rollback/rewind the model tried something different
or repeated the undone attempt. Copy the lesson text if there was one.

## 5. Branch search with a real model (10 min, ~4× the tokens of one run)

```sh
cd ~/rune-fixtures/calc && git checkout -q . && git clean -fdq
rune search --check "python3 test_visible.py" \
  --hidden ~/rune-fixtures/calc-hidden --hidden-cmd "python3 test_hidden.py" \
  --hyp "fix the arithmetic in add" \
  --hyp "make the failing test pass with the smallest possible change" \
  --hyp "check whether the test itself is wrong" \
  --hyp "rewrite add from scratch" \
  "add() is wrong; fix it"
rune heads
```

Expect: a table of 4 attempts with visible and hidden scores, one marked ★, merged into the worktree.

Note: do the attempts actually differ (look at `rune -b search/<id>/<i> transcript` for two of them)? Did any
attempt game the visible test? How long did it take compared with one `rune run`?

## 6. Your own repository (open-ended)

```sh
cd <a scratch copy of a real repo>
rune bench -n 10                 # snapshot cost: decides per-call vs per-turn
rune chat --mode ask             # a small real task you know the answer to
```

Note: the `rune bench` p95, and anything that felt wrong or slow.

## What to send back

For each step: worked / failed / surprised, plus

- any error output, whole;
- `rune verify` output;
- the `[… · cache read … (x%) …]` lines;
- for steps 3–5, your answer to the "Note" questions.

Known gaps you may hit: no Ctrl-C inside a turn (kill the process; `rune chat -r` resumes), no
compaction (very long sessions will eventually hit the context limit), and search forks map the main path
only in tool arguments, output and command strings.
