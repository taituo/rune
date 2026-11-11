#!/usr/bin/env bash
# Creates small test projects for trying rune with a real model.
#   scripts/fixtures.sh DIR
# DIR/calc         add() is wrong; one visible test
# DIR/calc-hidden  held-out tests for `rune search --hidden`
# DIR/trap         a failure whose error message points at the wrong file
# Needs only python3 (no pytest). Each test prints "N passed, M failed", which rune's checks parse.
set -euo pipefail
dir=${1:?usage: fixtures.sh DIR}
mkdir -p "$dir"
cd "$dir"

runner='import sys
def run(cases, ns):
    p = f = 0
    for expr, want in cases:
        try:
            ok = eval(expr, ns) == want
        except Exception as e:
            ok = False
            print("ERROR", expr, type(e).__name__, e)
        if ok:
            p += 1
        else:
            f += 1
            print("FAIL", expr)
    print(f"{p} passed, {f} failed")
    sys.exit(1 if f else 0)
'

rm -rf calc calc-hidden trap
mkdir calc calc-hidden trap

# --- calc ---
printf '%s' "$runner" > calc/runner.py
cat > calc/calc.py <<'EOF'
def add(a, b):
    return a - b


def mul(a, b):
    return a * b
EOF
cat > calc/test_visible.py <<'EOF'
from runner import run
from calc import add, mul
run([("add(2, 2)", 4), ("mul(3, 4)", 12)], globals())
EOF
cat > calc-hidden/test_hidden.py <<'EOF'
from runner import run
from calc import add, mul
run([("add(2, 2)", 4), ("add(3, 5)", 8), ("add(-1, 1)", 0), ("add(10, -4)", 6), ("mul(3, 4)", 12)], globals())
EOF

# --- trap: the error blames the client; the cause is the config ---
printf '%s' "$runner" > trap/runner.py
cat > trap/config.py <<'EOF'
# Request settings
RETRIES = 3
TIMEOUT_SECONDS = 0  # set by a migration script
EOF
cat > trap/client.py <<'EOF'
import config


class ClientTimeout(Exception):
    pass


def fetch(path):
    for attempt in range(config.RETRIES):
        if config.TIMEOUT_SECONDS > 0:
            return f"ok:{path}"
    raise ClientTimeout(f"client.fetch({path!r}): timed out after {config.TIMEOUT_SECONDS}s, retries exhausted")
EOF
cat > trap/test_client.py <<'EOF'
from runner import run
from client import fetch
run([("fetch('/a')", "ok:/a"), ("fetch('/b')", "ok:/b")], globals())
EOF

for p in calc trap; do
  (cd "$p" && git init -q && git add -A && git -c user.name=fixture -c user.email=fixture@localhost commit -qm "fixture")
done
echo "created $dir/calc, $dir/calc-hidden, $dir/trap"
