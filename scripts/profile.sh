#!/usr/bin/env bash
# Profile a runic-bench Criterion target with perf (+ optional flamegraph/samply).
#
# Builds once, resolves the exact bench executable, then runs that binary
# directly under perf (no cargo/rustc in the measured region).
#
# Default pipeline: build → resolve → perf stat → perf record → reports → flamegraph → summary
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/profile.sh [options] <bench-target> <criterion-filter>
       scripts/profile.sh --compare <before-dir> <after-dir>

Positional:
  bench-target       Criterion bench binary (explicit, threaded, compare_explicit, ...)
  criterion-filter   Exact Criterion filter (e.g. explicit/single_size_churn/runic/64)

Options:
  -t, --time SEC         Criterion --profile-time seconds (default: 30)
  -a, --annotate SYM     perf annotate for SYM (also positional arg 4)
  -o, --output-dir DIR   Profile output root for this run
  -l, --label NAME       Manifest label (e.g. before, after, baseline)
  --compare A B          Diff two profile dirs' metrics.txt (ratios + % delta)
  --with LIST            Extra/optional tools, comma-separated:
                           flamegraph (default on when available)
                           samply     (Firefox Profiler JSON; skipped if missing)
                           none       disable extras
  --skip LIST            Skip stages, comma-separated:
                           build,stat,record,report,flamegraph,samply,annotate,summary
  -h, --help             Show this help

Environment:
  RUNIC_PROFILE_DIR            Output root (default: <repo>/target/runic-profiles)
  RUNIC_PROFILE_FREQ           perf sample frequency (default: 997)
  RUNIC_PROFILE_STAT_REPEATS   perf stat -r (default: 5)
  RUNIC_PROFILE_STAT_SECONDS   Criterion --profile-time for perf stat (default: 5)
  RUNIC_PROFILE_EVENT          Preferred record event (default: cycles:u)
  RUNIC_PROFILE_FALLBACK_EVENT Fallback record event (default: cpu-clock:u)
  CARGO_PROFILE_BENCH_DEBUG    Passed through (default: line-tables-only)
  RUSTFLAGS                    Frame pointers forced on if missing

Examples:
  scripts/profile.sh explicit 'explicit/single_size_churn/runic/64'
  scripts/profile.sh -l baseline -t 20 \
    threaded 'threaded/persistent_remote_fan_in/runic/4/live:256'
  scripts/profile.sh -a 'ThreadHeap>::alloc' \
    explicit 'explicit/single_size_churn/runic/64'
  scripts/profile.sh --compare \
    target/runic-profiles/foo-before target/runic-profiles/foo-after
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

warn() {
  printf 'warning: %s\n' "$*" >&2
}

info() {
  printf '%s\n' "$*"
}

have_cmd() {
  command -v "$1" >/dev/null 2>&1
}

require_cmd() {
  have_cmd "$1" || die "missing required command: $1"
}

want_stage() {
  local stage=$1
  case ",${SKIP_STAGES}," in
    *",${stage},"*) return 1 ;;
    *) return 0 ;;
  esac
}

want_tool() {
  local tool=$1
  case ",${WITH_TOOLS}," in
    *",${tool},"*) return 0 ;;
    *) return 1 ;;
  esac
}

print_command() {
  local arg
  for arg in "$@"; do
    printf '%q ' "$arg"
  done
  printf '\n'
}

csv_has() {
  local needle=$1
  local hay=$2
  case ",${hay}," in
    *",${needle},"*) return 0 ;;
    *) return 1 ;;
  esac
}

normalize_csv() {
  printf '%s' "$1" | tr '[:upper:]' '[:lower:]' | tr -s ' ,' ',' | sed 's/^,//;s/,$//'
}

PROFILE_SECONDS=30
HOT_SYMBOL=
OUTPUT_ROOT_OVERRIDE=
RUN_LABEL=
WITH_TOOLS=flamegraph
SKIP_STAGES=
BENCH_TARGET=
CRITERION_FILTER=
COMPARE_BEFORE=
COMPARE_AFTER=

while [[ $# -gt 0 ]]; do
  case $1 in
    -h | --help)
      usage
      exit 0
      ;;
    -t | --time)
      [[ $# -ge 2 ]] || die "$1 requires a value"
      PROFILE_SECONDS=$2
      shift 2
      ;;
    -a | --annotate)
      [[ $# -ge 2 ]] || die "$1 requires a value"
      HOT_SYMBOL=$2
      shift 2
      ;;
    -o | --output-dir)
      [[ $# -ge 2 ]] || die "$1 requires a value"
      OUTPUT_ROOT_OVERRIDE=$2
      shift 2
      ;;
    -l | --label)
      [[ $# -ge 2 ]] || die "$1 requires a value"
      RUN_LABEL=$2
      shift 2
      ;;
    --compare)
      [[ $# -ge 3 ]] || die "$1 requires <before-dir> <after-dir>"
      COMPARE_BEFORE=$2
      COMPARE_AFTER=$3
      shift 3
      ;;
    --with)
      [[ $# -ge 2 ]] || die "$1 requires a value"
      WITH_TOOLS=$(normalize_csv "$2")
      shift 2
      ;;
    --skip)
      [[ $# -ge 2 ]] || die "$1 requires a value"
      SKIP_STAGES=$(normalize_csv "$2")
      shift 2
      ;;
    --)
      shift
      break
      ;;
    -*)
      die "unknown option: $1 (try --help)"
      ;;
    *)
      break
      ;;
  esac
done

compare_metrics() {
  local before=$1
  local after=$2
  [[ -f $before/metrics.txt ]] || die "missing $before/metrics.txt"
  [[ -f $after/metrics.txt ]] || die "missing $after/metrics.txt"
  python3 - "$before" "$after" <<'PY'
import sys
from pathlib import Path

before_dir, after_dir = Path(sys.argv[1]), Path(sys.argv[2])

def load_kv(path: Path) -> dict[str, str]:
    out = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            out[k.strip()] = v.strip()
    return out

before = load_kv(before_dir / "metrics.txt")
after = load_kv(after_dir / "metrics.txt")
manifest_b = load_kv(before_dir / "manifest.txt") if (before_dir / "manifest.txt").exists() else {}
manifest_a = load_kv(after_dir / "manifest.txt") if (after_dir / "manifest.txt").exists() else {}

keys = [
    "cycles_per_elem",
    "instructions_per_elem",
    "branches_per_elem",
    "elems_per_sec",
    "ipc",
    "mean_reuse_ns",
    "cycles",
    "instructions",
    "seconds",
]

print("Runic profile compare")
print("=====================")
print(f"before: {before_dir}")
print(f"  label={manifest_b.get('label', '')} sha={manifest_b.get('git_sha', '')} filter={manifest_b.get('criterion_filter', '')}")
print(f"  binary={manifest_b.get('bench_bin', '')}")
print(f"after:  {after_dir}")
print(f"  label={manifest_a.get('label', '')} sha={manifest_a.get('git_sha', '')} filter={manifest_a.get('criterion_filter', '')}")
print(f"  binary={manifest_a.get('bench_bin', '')}")
print()
print(f"{'metric':<28} {'before':>14} {'after':>14} {'ratio':>10} {'delta%':>10}")
print("-" * 80)

for key in keys:
    if key not in before or key not in after:
        continue
    try:
        b = float(before[key])
        a = float(after[key])
    except ValueError:
        continue
    if b == 0:
        continue
    # Higher elems_per_sec / ipc is better → invert ratio display sense via delta only.
    ratio = a / b
    delta = (a - b) / b * 100.0
    print(f"{key:<28} {b:14.4f} {a:14.4f} {ratio:10.4f} {delta:9.2f}%")
PY
}

if [[ -n $COMPARE_BEFORE ]]; then
  [[ $# -eq 0 ]] || die "unexpected arguments after --compare: $*"
  compare_metrics "$COMPARE_BEFORE" "$COMPARE_AFTER"
  exit 0
fi

[[ $# -ge 2 ]] || {
  usage >&2
  exit 2
}
BENCH_TARGET=$1
CRITERION_FILTER=$2
shift 2
if [[ $# -ge 1 ]]; then
  PROFILE_SECONDS=$1
  shift
fi
if [[ $# -ge 1 ]]; then
  HOT_SYMBOL=$1
  shift
fi
[[ $# -eq 0 ]] || die "unexpected arguments: $*"

[[ $PROFILE_SECONDS =~ ^[0-9]+$ ]] || die "profile-seconds must be an integer"

if csv_has none "$WITH_TOOLS"; then
  WITH_TOOLS=
fi

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
cd "$REPO_ROOT"

OUTPUT_ROOT=${OUTPUT_ROOT_OVERRIDE:-${RUNIC_PROFILE_DIR:-$REPO_ROOT/target/runic-profiles}}
SAMPLE_FREQ=${RUNIC_PROFILE_FREQ:-997}
STAT_REPEATS=${RUNIC_PROFILE_STAT_REPEATS:-5}
# Fixed Criterion window for perf stat so repeated runs compare equal work.
STAT_PROFILE_SECONDS=${RUNIC_PROFILE_STAT_SECONDS:-5}
RECORD_EVENT=${RUNIC_PROFILE_EVENT:-cycles:u}
FALLBACK_EVENT=${RUNIC_PROFILE_FALLBACK_EVENT:-cpu-clock:u}

[[ $SAMPLE_FREQ =~ ^[0-9]+$ ]] || die "RUNIC_PROFILE_FREQ must be an integer"
[[ $STAT_REPEATS =~ ^[0-9]+$ ]] || die "RUNIC_PROFILE_STAT_REPEATS must be an integer"
[[ $STAT_PROFILE_SECONDS =~ ^[0-9]+$ ]] || die "RUNIC_PROFILE_STAT_SECONDS must be an integer"

require_cmd cargo
require_cmd rustc
require_cmd git
require_cmd perf
require_cmd uname
require_cmd date
require_cmd tr
require_cmd sed
require_cmd mktemp
require_cmd python3

STAT_EVENTS_PRIMARY='task-clock,cycles:u,instructions:u,branches:u,branch-misses:u,cache-references:u,cache-misses:u,page-faults,minor-faults,major-faults'
STAT_EVENTS_EXTENDED="${STAT_EVENTS_PRIMARY},dTLB-loads:u,dTLB-load-misses:u,L1-dcache-loads:u,L1-dcache-load-misses:u"

export CARGO_PROFILE_BENCH_DEBUG=${CARGO_PROFILE_BENCH_DEBUG:-line-tables-only}
case " ${RUSTFLAGS:-} " in
  *' -C force-frame-pointers=yes '*) ;;
  *)
    if [[ -n ${RUSTFLAGS:-} ]]; then
      export RUSTFLAGS="$RUSTFLAGS -C force-frame-pointers=yes"
    else
      export RUSTFLAGS='-C force-frame-pointers=yes'
    fi
    ;;
esac

GIT_SHA=$(git rev-parse --short=12 HEAD 2>/dev/null || printf 'unknown')
if git diff --quiet --ignore-submodules -- 2>/dev/null && git diff --cached --quiet --ignore-submodules -- 2>/dev/null; then
  GIT_DIRTY=no
else
  GIT_DIRTY=yes
fi

if [[ $CRITERION_FILTER == *setup_lifecycle* ]]; then
  warn "setup_lifecycle filters intentionally include spawn/join/unbind noise"
elif [[ $BENCH_TARGET == *threaded* && $CRITERION_FILTER != *persistent_* ]]; then
  warn "non-persistent threaded filters may include thread-setup noise; prefer persistent_* "
fi

timestamp=$(date +%Y%m%d-%H%M%S)
slug=$(printf '%s-%s' "$BENCH_TARGET" "$CRITERION_FILTER" | tr -cs '[:alnum:]_.-' '-')
if [[ -n $RUN_LABEL ]]; then
  label_slug=$(printf '%s' "$RUN_LABEL" | tr -cs '[:alnum:]_.-' '-')
  OUT_DIR="$OUTPUT_ROOT/$timestamp-$GIT_SHA-$label_slug-$slug"
else
  OUT_DIR="$OUTPUT_ROOT/$timestamp-$GIT_SHA-$slug"
fi
mkdir -p "$OUT_DIR"

resolve_bench_bin() {
  local target=$1
  local json_log=$2
  local bin

  cargo bench -p runic-bench --bench "$target" --no-run --message-format=json \
    >"$json_log" 2>"${json_log}.stderr"

  bin=$(
    python3 - "$json_log" "$target" <<'PY'
import json, os, sys
path, name = sys.argv[1], sys.argv[2]
candidates = []
with open(path, encoding="utf-8") as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        if msg.get("reason") != "compiler-artifact":
            continue
        target = msg.get("target") or {}
        if target.get("name") != name:
            continue
        if "bench" not in (target.get("kind") or []):
            continue
        candidate = msg.get("executable")
        if not candidate:
            continue
        candidates.append((bool(msg.get("fresh")), candidate))
if not candidates:
    sys.exit(1)
# Prefer a freshly built artifact; otherwise the newest mtime (stale fingerprints linger).
fresh = [c for is_fresh, c in candidates if is_fresh]
pool = fresh or [c for _, c in candidates]
pool = [c for c in pool if os.path.isfile(c)]
if not pool:
    sys.exit(1)
print(max(pool, key=lambda p: os.path.getmtime(p)))
PY
  ) || die "failed to resolve bench executable for $target (see $json_log)"

  [[ -x $bin ]] || die "resolved bench binary is not executable: $bin"
  printf '%s\n' "$bin"
}

run_perf_stat() {
  local output_file=$1
  shift

  {
    printf '# perf stat (extended allocator counters)\n'
    printf '# command: '
    print_command perf stat -r "$STAT_REPEATS" -e "$STAT_EVENTS_EXTENDED" -- "$@"
    printf '\n'
  } >"$output_file"

  if perf stat -r "$STAT_REPEATS" -e "$STAT_EVENTS_EXTENDED" -- "$@" >>"$output_file" 2>&1; then
    return 0
  fi

  warn "extended perf stat events failed; retrying primary set"
  {
    printf '\n# extended events failed; primary set\n'
    printf '# command: '
    print_command perf stat -r "$STAT_REPEATS" -e "$STAT_EVENTS_PRIMARY" -- "$@"
    printf '\n'
  } >>"$output_file"

  if perf stat -r "$STAT_REPEATS" -e "$STAT_EVENTS_PRIMARY" -- "$@" >>"$output_file" 2>&1; then
    return 0
  fi

  warn "primary perf stat failed; retrying perf stat -d"
  {
    printf '\n# primary events failed; fallback to perf stat -d\n'
    printf '# command: '
    print_command perf stat -r "$STAT_REPEATS" -d -- "$@"
    printf '\n'
  } >>"$output_file"

  if perf stat -r "$STAT_REPEATS" -d -- "$@" >>"$output_file" 2>&1; then
    return 0
  fi

  warn "perf stat failed entirely; continuing without counters"
  return 0
}

record_profile() {
  local output_file=$1
  local event_file=$2
  shift 2

  local record_log="${output_file}.log"
  {
    printf '# perf record preferred event\n'
    printf '# command: '
    print_command perf record -F "$SAMPLE_FREQ" -e "$RECORD_EVENT" -g --call-graph fp -o "$output_file" -- "$@"
    printf '\n'
  } >"$record_log"

  if perf record -F "$SAMPLE_FREQ" -e "$RECORD_EVENT" -g --call-graph fp -o "$output_file" -- "$@" >>"$record_log" 2>&1; then
    printf '%s\n' "$RECORD_EVENT" >"$event_file"
    return 0
  fi

  warn "perf record with $RECORD_EVENT failed; retrying with $FALLBACK_EVENT"
  {
    printf '\n# preferred event failed; fallback event\n'
    printf '# command: '
    print_command perf record -F "$SAMPLE_FREQ" -e "$FALLBACK_EVENT" -g --call-graph fp -o "$output_file" -- "$@"
    printf '\n'
  } >>"$record_log"

  perf record -F "$SAMPLE_FREQ" -e "$FALLBACK_EVENT" -g --call-graph fp -o "$output_file" -- "$@" >>"$record_log" 2>&1
  printf '%s\n' "$FALLBACK_EVENT" >"$event_file"
}

write_flamegraph() {
  local perf_data=$1
  local svg=$2
  local log=$3

  have_cmd flamegraph || {
    warn "flamegraph not installed; skip SVG (cargo install flamegraph)"
    return 0
  }

  {
    printf '# flamegraph from existing perf.data\n'
    printf '# command: '
    print_command flamegraph --perfdata "$perf_data" -o "$svg" --palette rust \
      --title "runic $BENCH_TARGET" --subtitle "$CRITERION_FILTER"
    printf '\n'
  } >"$log"

  if flamegraph --perfdata "$perf_data" -o "$svg" --palette rust \
    --title "runic $BENCH_TARGET" --subtitle "$CRITERION_FILTER" >>"$log" 2>&1; then
    info "Wrote $svg"
  else
    warn "flamegraph generation failed; see $log"
  fi
}

write_samply() {
  local perf_data=$1
  local out_json=$2
  local log=$3

  have_cmd samply || {
    warn "samply not installed; skip (cargo install samply)"
    return 0
  }

  {
    printf '# samply import of perf.data\n'
    printf '# command: '
    print_command samply import "$perf_data" --save-only -o "$out_json"
    printf '\n'
  } >"$log"

  if samply import "$perf_data" --save-only -o "$out_json" >>"$log" 2>&1; then
    info "Wrote $out_json (open via: samply load $out_json)"
    return 0
  fi

  if samply import "$perf_data" -o "$out_json" >>"$log" 2>&1; then
    info "Wrote $out_json"
    return 0
  fi

  warn "samply import failed; see $log (try: samply import $perf_data)"
}

write_metrics() {
  local output_file=$1
  python3 - "$OUT_DIR/perf-stat.txt" "$output_file" <<'PY'
import re, sys
stat_path, out_path = sys.argv[1], sys.argv[2]
text = open(stat_path, encoding="utf-8", errors="replace").read()

def first_number(pattern: str):
    m = re.search(pattern, text)
    if not m:
        return None
    return float(m.group(1).replace(",", ""))

cycles = first_number(r"([\d,]+)\s+cycles:u")
insns = first_number(r"([\d,]+)\s+instructions:u")
branches = first_number(r"([\d,]+)\s+branches:u")
cache_refs = first_number(r"([\d,]+)\s+cache-references:u")
cache_misses = first_number(r"([\d,]+)\s+cache-misses:u")
task = first_number(r"([\d,.]+)\s+msec task-clock")

elems_per_s = None
# Prefer absolute thrpt lines; skip Criterion "change" percentage blocks.
# Criterion scales units: Kelem/s, Melem/s, Gelem/s.
triples = re.findall(
    r"thrpt:\s*\[([\d.]+)\s*([KMG]?)elem/s\s+([\d.]+)\s*([KMG]?)elem/s\s+([\d.]+)\s*([KMG]?)elem/s\]",
    text,
)
scale = {"": 1.0, "K": 1e3, "M": 1e6, "G": 1e9}
if triples:
    medians = [float(t[2]) * scale.get(t[3], 1.0) for t in triples]
    elems_per_s = medians[len(medians) // 2]

seconds = None
m = re.search(r"([\d.]+)\s+\+-.*?seconds time elapsed", text)
if m:
    seconds = float(m.group(1))
elif task is not None:
    seconds = task / 1000.0

mean_reuse = None
reuse_vals = [float(v) for v in re.findall(r"runic_mean_reuse_ns=(\d+(?:\.\d+)?)", text)]
if reuse_vals:
    mean_reuse = reuse_vals[-1]

lines = []
if cycles is not None:
    lines.append(f"cycles={cycles:.0f}")
if insns is not None:
    lines.append(f"instructions={insns:.0f}")
if branches is not None:
    lines.append(f"branches={branches:.0f}")
if cache_refs is not None:
    lines.append(f"cache_references={cache_refs:.0f}")
if cache_misses is not None:
    lines.append(f"cache_misses={cache_misses:.0f}")
if seconds is not None and seconds > 0:
    lines.append(f"seconds={seconds:.6f}")
    if cycles is not None:
        lines.append(f"cycles_per_sec={cycles / seconds:.3f}")
if elems_per_s is not None:
    lines.append(f"elems_per_sec={elems_per_s:.3f}")
    if seconds is not None and seconds > 0:
        total_elems = elems_per_s * seconds
        if total_elems > 0:
            if cycles is not None:
                lines.append(f"cycles_per_elem={cycles / total_elems:.3f}")
            if insns is not None:
                lines.append(f"instructions_per_elem={insns / total_elems:.3f}")
            if branches is not None:
                lines.append(f"branches_per_elem={branches / total_elems:.3f}")
            if cache_misses is not None:
                lines.append(f"cache_misses_per_elem={cache_misses / total_elems:.6f}")
if cycles is not None and insns is not None and cycles > 0:
    lines.append(f"ipc={insns / cycles:.4f}")
if mean_reuse is not None:
    lines.append(f"mean_reuse_ns={mean_reuse:.0f}")

open(out_path, "w", encoding="utf-8").write("\n".join(lines) + ("\n" if lines else ""))
PY
}

write_summary() {
  local output_file=$1
  local event_used=$2
  local tmp

  tmp=$(mktemp)
  {
    printf '%s\n' 'Runic profile summary' '====================='
    printf 'filter:  %s / %s\n' "$BENCH_TARGET" "$CRITERION_FILTER"
    printf 'label:   %s\n' "${RUN_LABEL:-none}"
    printf 'binary:  %s\n' "$BENCH_BIN"
    printf 'event:   %s @ %s Hz for %ss\n' "$event_used" "$SAMPLE_FREQ" "$PROFILE_SECONDS"
    printf 'git:     %s dirty=%s\n' "$GIT_SHA" "$GIT_DIRTY"
    printf 'out:     %s\n' "$OUT_DIR"
    printf '\n'

    if [[ -f $OUT_DIR/metrics.txt ]]; then
      printf '%s\n' '--- derived metrics ---'
      cat "$OUT_DIR/metrics.txt"
      printf '\n'
    fi

    if [[ -f $OUT_DIR/perf-stat.txt ]]; then
      printf '%s\n' '--- perf stat (tail) ---'
      tail -n 40 "$OUT_DIR/perf-stat.txt"
      printf '\n'
    fi

    if [[ -f $OUT_DIR/perf-report-flat.txt ]]; then
      printf '%s\n' '--- top flat symbols (% limit 0.3) ---'
      awk '
        BEGIN { n = 0 }
        /^#/ { if ($0 ~ /Overhead|Samples|Event/) print; next }
        /^$/ { next }
        {
          print
          if (++n >= 30) exit
        }
      ' "$OUT_DIR/perf-report-flat.txt"
      printf '\n'
    fi

    printf '%s\n' 'Artifacts:'
    for f in metadata.txt manifest.txt command.txt metrics.txt perf-stat.txt perf.data \
      perf-report-flat.txt perf-report-self.txt perf-report-children.txt \
      flamegraph.svg flamegraph.log samply.json samply.log \
      perf-annotate.txt summary.txt; do
      if [[ -e $OUT_DIR/$f ]]; then
        printf '  %s\n' "$f"
      fi
    done
  } >"$tmp"
  mv "$tmp" "$output_file"
}

write_metadata() {
  local output_file=$1
  local event_used=$2

  {
    printf 'date=%s\n' "$(date -Is)"
    printf 'repo=%s\n' "$REPO_ROOT"
    printf 'branch=%s\n' "$(git rev-parse --abbrev-ref HEAD 2>/dev/null || printf 'unknown')"
    printf 'git_sha=%s\n' "$GIT_SHA"
    printf 'git_dirty=%s\n' "$GIT_DIRTY"
    printf 'label=%s\n' "${RUN_LABEL:-}"
    printf 'bench_target=%s\n' "$BENCH_TARGET"
    printf 'criterion_filter=%s\n' "$CRITERION_FILTER"
    printf 'bench_bin=%s\n' "$BENCH_BIN"
    printf 'profile_seconds=%s\n' "$PROFILE_SECONDS"
    printf 'stat_profile_seconds=%s\n' "$STAT_PROFILE_SECONDS"
    printf 'sample_frequency=%s\n' "$SAMPLE_FREQ"
    printf 'stat_repeats=%s\n' "$STAT_REPEATS"
    printf 'record_event=%s\n' "$event_used"
    printf 'with_tools=%s\n' "$WITH_TOOLS"
    printf 'skip_stages=%s\n' "$SKIP_STAGES"
    printf 'hot_symbol=%s\n' "${HOT_SYMBOL:-}"
    printf 'cargo_profile_bench_debug=%s\n' "$CARGO_PROFILE_BENCH_DEBUG"
    printf 'rustflags=%s\n' "$RUSTFLAGS"
    printf 'rustc=%s\n' "$(rustc --version)"
    printf 'cargo=%s\n' "$(cargo --version)"
    printf 'perf=%s\n' "$(perf version)"
    if have_cmd flamegraph; then
      printf 'flamegraph=%s\n' "$(flamegraph --version 2>/dev/null || printf 'present')"
    fi
    if have_cmd samply; then
      printf 'samply=%s\n' "$(samply --version 2>/dev/null || printf 'present')"
    fi
    printf 'kernel=%s\n' "$(uname -a)"
    printf 'cpu_model=%s\n' "$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ //' || printf unknown)"
    printf '\n[git status --short]\n'
    git status --short
  } >"$output_file"
}

write_manifest() {
  local output_file=$1
  local event_used=$2
  {
    printf 'kind=runic-profile-manifest\n'
    printf 'version=1\n'
    printf 'label=%s\n' "${RUN_LABEL:-}"
    printf 'git_sha=%s\n' "$GIT_SHA"
    printf 'git_dirty=%s\n' "$GIT_DIRTY"
    printf 'bench_target=%s\n' "$BENCH_TARGET"
    printf 'criterion_filter=%s\n' "$CRITERION_FILTER"
    printf 'bench_bin=%s\n' "$BENCH_BIN"
    printf 'out_dir=%s\n' "$OUT_DIR"
    printf 'record_event=%s\n' "$event_used"
    printf 'profile_seconds=%s\n' "$PROFILE_SECONDS"
    printf 'stat_repeats=%s\n' "$STAT_REPEATS"
  } >"$output_file"
}

BENCH_BIN=
EVENT_USED=none

STAT_ARGS=()
PROFILE_ARGS=()

if want_stage build; then
  info "Building optimized bench with profiling metadata..."
  BENCH_BIN=$(resolve_bench_bin "$BENCH_TARGET" "$OUT_DIR/cargo-build.json")
else
  # Still need a binary for later stages.
  BENCH_BIN=$(resolve_bench_bin "$BENCH_TARGET" "$OUT_DIR/cargo-build.json")
fi
info "Resolved bench binary: $BENCH_BIN"

STAT_ARGS=("$BENCH_BIN" "$CRITERION_FILTER" --exact --warm-up-time 0.25 --measurement-time "$STAT_PROFILE_SECONDS" --sample-size 10 --noplot --bench)
PROFILE_ARGS=("$BENCH_BIN" "$CRITERION_FILTER" --exact --profile-time "$PROFILE_SECONDS" --noplot --bench --quiet)

{
  printf '[build]\n'
  print_command cargo bench -p runic-bench --bench "$BENCH_TARGET" --no-run --message-format=json
  printf 'resolved_bin=%s\n' "$BENCH_BIN"
  printf '\n[perf stat]\n'
  print_command perf stat -r "$STAT_REPEATS" -e "$STAT_EVENTS_EXTENDED" -- "${STAT_ARGS[@]}"
  printf '\n[perf record]\n'
  print_command perf record -F "$SAMPLE_FREQ" -e "$RECORD_EVENT" -g --call-graph fp -o "$OUT_DIR/perf.data" -- "${PROFILE_ARGS[@]}"
  if want_tool flamegraph; then
    printf '\n[flamegraph]\n'
    print_command flamegraph --perfdata "$OUT_DIR/perf.data" -o "$OUT_DIR/flamegraph.svg" --palette rust
  fi
  if want_tool samply; then
    printf '\n[samply]\n'
    print_command samply import "$OUT_DIR/perf.data" -o "$OUT_DIR/samply.json"
  fi
} >"$OUT_DIR/command.txt"

if want_stage stat; then
  info "Running perf stat on resolved binary..."
  run_perf_stat "$OUT_DIR/perf-stat.txt" "${STAT_ARGS[@]}"
  write_metrics "$OUT_DIR/metrics.txt" || warn "metrics derivation failed"
fi

if want_stage record; then
  info "Recording profile (${PROFILE_SECONDS}s) on resolved binary..."
  record_profile "$OUT_DIR/perf.data" "$OUT_DIR/event-used.txt" "${PROFILE_ARGS[@]}"
  EVENT_USED=$(<"$OUT_DIR/event-used.txt")
elif [[ -f $OUT_DIR/event-used.txt ]]; then
  EVENT_USED=$(<"$OUT_DIR/event-used.txt")
fi

if want_stage report; then
  [[ -f $OUT_DIR/perf.data ]] || die "missing perf.data; cannot report (skip record?)"
  info "Generating perf reports..."
  perf report --stdio --no-children --percent-limit 0.5 -i "$OUT_DIR/perf.data" --sort=dso,symbol \
    >"$OUT_DIR/perf-report-self.txt"
  perf report --stdio --children --percent-limit 0.5 -i "$OUT_DIR/perf.data" --sort=dso,symbol \
    >"$OUT_DIR/perf-report-children.txt"
  perf report --stdio --no-children -g none --percent-limit 0.3 -i "$OUT_DIR/perf.data" \
    --sort=overhead,symbol >"$OUT_DIR/perf-report-flat.txt"
fi

if want_stage flamegraph && want_tool flamegraph; then
  [[ -f $OUT_DIR/perf.data ]] || die "missing perf.data; cannot flamegraph"
  info "Generating flamegraph..."
  write_flamegraph "$OUT_DIR/perf.data" "$OUT_DIR/flamegraph.svg" "$OUT_DIR/flamegraph.log"
fi

if want_stage samply && want_tool samply; then
  [[ -f $OUT_DIR/perf.data ]] || die "missing perf.data; cannot samply"
  info "Exporting samply profile..."
  write_samply "$OUT_DIR/perf.data" "$OUT_DIR/samply.json" "$OUT_DIR/samply.log"
fi

if want_stage annotate && [[ -n $HOT_SYMBOL ]]; then
  [[ -f $OUT_DIR/perf.data ]] || die "missing perf.data; cannot annotate"
  info "Annotating $HOT_SYMBOL..."
  annotate_symbol=$HOT_SYMBOL
  resolve_from=
  if [[ -f $OUT_DIR/perf-report-flat.txt ]]; then
    resolve_from=$OUT_DIR/perf-report-flat.txt
  elif [[ -f $OUT_DIR/perf-report-self.txt ]]; then
    resolve_from=$OUT_DIR/perf-report-self.txt
  fi
  if [[ -n $resolve_from ]]; then
    resolved=$(
      awk -v needle="$HOT_SYMBOL" '
        $0 !~ /^#/ && index($0, needle) {
          for (i = 1; i <= NF; i++) {
            if (index($i, needle)) {
              sym = $i
              sub(/^\[\.\]/, "", sym)
              print sym
              exit
            }
          }
        }
      ' "$resolve_from"
    )
    if [[ -n ${resolved:-} ]]; then
      annotate_symbol=$resolved
      info "Resolved annotate symbol to: $annotate_symbol"
    else
      warn "no profiled symbol contains '$HOT_SYMBOL'; annotate may find no samples"
    fi
  fi
  if ! perf annotate --stdio -i "$OUT_DIR/perf.data" --symbol "$annotate_symbol" \
    >"$OUT_DIR/perf-annotate.txt" 2>"$OUT_DIR/perf-annotate.err"; then
    warn "perf annotate failed for symbol: $annotate_symbol (see perf-annotate.err)"
    if [[ -s $OUT_DIR/perf-annotate.err ]]; then
      warn "$(head -n 3 "$OUT_DIR/perf-annotate.err")"
    fi
  fi
fi

write_metadata "$OUT_DIR/metadata.txt" "$EVENT_USED"
write_manifest "$OUT_DIR/manifest.txt" "$EVENT_USED"

if want_stage summary; then
  write_summary "$OUT_DIR/summary.txt" "$EVENT_USED"
  info ""
  info "======== summary ========"
  cat "$OUT_DIR/summary.txt"
  info "========================="
fi

info "Profile artifacts written to $OUT_DIR"
