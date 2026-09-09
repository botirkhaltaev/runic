#!/usr/bin/env bash
# Policy hillclimb: N repeats of metrics, median + spread, train/hold-out gates.
# minflt vs snmalloc is skipped when the snmalloc median is 0 (no signal).
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
cd "$REPO_ROOT"

N=${RUNIC_POLICY_GRID_N:-5}
OUT_DIR=${1:-$REPO_ROOT/target/runic-profiles/policy-grid}
mkdir -p "$OUT_DIR"

TARGETS=${RUNIC_POLICY_GRID_TARGETS:-runic,runic:keep/keep,runic:discard/keep,runic:keep/discard,runic:discard/discard,runic:unmap/keep,runic:keep/keep:tight,snmalloc}
TRAIN=${RUNIC_POLICY_GRID_TRAIN:-vec_push_clear,vec_many_small,string_building,hashmap_insert_remove,arc_clone_drop,mixed_collections,json_api,regex_search,http_buffers,channel_pipeline,arc_share_drop,scoped_map_reduce,large_buffers}
HOLDOUT=${RUNIC_POLICY_GRID_HOLDOUT:-tree,word_count,large_buffers_dirty,run_churn_bursty}

CASES="$TRAIN,$HOLDOUT"

echo "Building metrics..."
BIN=$(cargo build -p runic-bench --release --bin metrics --offline --message-format=json \
  | python3 -c '
import json, sys
found = None
for line in sys.stdin:
    try:
        msg = json.loads(line)
    except json.JSONDecodeError:
        continue
    if msg.get("reason") != "compiler-artifact":
        continue
    target = msg.get("target") or {}
    if target.get("name") != "metrics":
        continue
    exe = msg.get("executable")
    if exe:
        found = exe
if not found:
    sys.exit(1)
print(found)
')
[[ -x $BIN ]] || { echo "error: cannot find metrics binary" >&2; exit 1; }
echo "metrics: $BIN"

RAW=$OUT_DIR/raw.csv
: >"$RAW"
SYSCALLS=()
if perf stat -e syscalls:sys_enter_madvise -- sleep 0.01 >/dev/null 2>&1; then
  SYSCALLS=(--syscalls)
fi

echo "allocator,workload,repeat,elapsed_ns,rss_peak,rss_after_free,minflt,madvise" >"$RAW"
for ((i = 1; i <= N; i++)); do
  echo "repeat $i / $N"
  "$BIN" --targets "$TARGETS" --cases "$CASES" "${SYSCALLS[@]}" \
    | python3 -c '
import sys
rep = sys.argv[1]
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    cols = line.split(",")
    if cols[0] == "allocator":
        continue
    elapsed = cols[4]
    after = cols[7]
    minflt = cols[10]
    madvise = cols[14] if len(cols) > 14 else ""
    print(f"{cols[0]},{cols[1]},{rep},{elapsed},{cols[6]},{after},{minflt},{madvise}")
' "$i" >>"$RAW"
done

python3 - "$RAW" "$OUT_DIR/decision.txt" "$TRAIN" "$HOLDOUT" <<'PY'
import csv, math, statistics, sys
from collections import defaultdict

raw, out_path, train_s, hold_s = sys.argv[1:5]
train = set(train_s.split(","))
hold = set(hold_s.split(","))

rows = list(csv.DictReader(open(raw, encoding="utf-8")))
by = defaultdict(list)
for r in rows:
    by[(r["allocator"], r["workload"])].append(r)

def nums(xs, key):
    vals = []
    for x in xs:
        v = x[key]
        if v == "":
            continue
        vals.append(float(v))
    return vals

def median(xs):
    return statistics.median(xs) if xs else float("nan")

workloads = sorted({w for _, w in by}, key=lambda w: (w not in train, w))
allocs = sorted({a for a, _ in by})

print("Policy grid")
print("===========")
print(f"repeats={len({r['repeat'] for r in rows})} train={','.join(sorted(train))}")
print(f"holdout={','.join(sorted(hold))}")
print()

tables = {}
for w in workloads:
    kind = "train" if w in train else "hold-out"
    print(f"## {w} ({kind})")
    print(f"{'allocator':<24} {'elapsed':>12} {'spread%':>8} {'rss_after':>12} {'minflt':>8} {'madvise':>8}")
    for a in allocs:
        xs = by.get((a, w), [])
        ev = nums(xs, "elapsed_ns")
        after = nums(xs, "rss_after_free")
        flt = nums(xs, "minflt")
        mad = nums(xs, "madvise")
        if not ev:
            continue
        med = median(ev)
        spread = (max(ev) - min(ev)) / med * 100 if med else 0
        tables[(a, w)] = {
            "elapsed": med,
            "spread": spread,
            "rss": median(after) if after else float("nan"),
            "minflt": median(flt) if flt else float("nan"),
            "madvise": median(mad) if mad else float("nan"),
        }
        print(f"{a:<24} {med:12.0f} {spread:8.2f} {tables[(a,w)]['rss']:12.0f} {tables[(a,w)]['minflt']:8.0f} {tables[(a,w)]['madvise']:8.0f}")
    print()

def ratio(a, b):
    if b == 0 or math.isnan(a) or math.isnan(b):
        return float("nan")
    return a / b

incumbent = "runic"
sn = "snmalloc"
candidates = [a for a in allocs if a.startswith("runic")]

print("## Decision")
lines = []
for a in candidates:
    guard_fail = []
    train_reg = []
    hold_reg = []
    train_ratios = []
    for w in workloads:
        t = tables.get((a, w))
        s = tables.get((sn, w))
        i = tables.get((incumbent, w))
        if not t or not s or not i:
            continue
        if t["rss"] > 1.10 * s["rss"] + 1:
            guard_fail.append(f"{w} rss {t['rss']:.0f}/{s['rss']:.0f}")
        # snmalloc minflt is often 0 — no signal, not a universal veto.
        if s["minflt"] > 0 and t["minflt"] > 1.10 * s["minflt"] + 1:
            guard_fail.append(f"{w} minflt {t['minflt']:.0f}/{s['minflt']:.0f}")
        r = ratio(t["elapsed"], i["elapsed"])
        if w in train:
            train_ratios.append(r)
            if r > 1.05:
                train_reg.append(f"{w} {r:.3f}")
        if w in hold and r > 1.05:
            hold_reg.append(f"{w} {r:.3f}")
    if not train_ratios:
        continue
    geo = math.exp(sum(math.log(r) for r in train_ratios if r > 0) / len(train_ratios))
    win = geo <= 0.95 and not train_reg and not hold_reg and not guard_fail
    status = "WIN" if win else "keep incumbent"
    msg = f"{a}: geomean vs {incumbent}={geo:.3f} guard_fail={guard_fail or '-'} train_reg={train_reg or '-'} hold_reg={hold_reg or '-'} -> {status}"
    print(msg)
    lines.append(msg)

open(out_path, "w", encoding="utf-8").write("\n".join(lines) + "\n")
PY

echo "Wrote $RAW and $OUT_DIR/decision.txt"
