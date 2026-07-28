#!/usr/bin/env bash
# Profile a runic-bench Criterion target with standard tools on the resolved ELF.
#
# Builds once, resolves the bench executable, pins CPUs, then runs that binary
# under perf (and optional flamegraph / samply / callgrind). Criterion benches
# stay ordinary timing code; this script only orchestrates measurement.
#
# Evidence model (Cost → Where → Why → Causal):
#   Cost   — metrics.txt from perf stat + Criterion thrpt; --compare
#   Where  — perf report / annotate / samply / flamegraph on perf.data
#   Why    — counter groups in perf-stat.txt; threaded phase filters
#   Causal — temporary A/B after reading Where; re-measure Cost
#
# Default: bootstrap → build → perf stat → perf record → reports → summary
set -euo pipefail

# ---------------------------------------------------------------------------
# utilities
# ---------------------------------------------------------------------------

usage() {
  cat <<'EOF'
Usage: scripts/profile.sh [options] <bench-target> <criterion-filter>
       scripts/profile.sh --compare <before-dir> <after-dir>
       scripts/profile.sh --preflight

Positional:
  bench-target       Criterion bench binary (explicit, threaded, compare_explicit, ...)
  criterion-filter   Exact Criterion filter (e.g. explicit/single_size_churn/runic/64)

Options:
  -t, --time SEC         Criterion --profile-time seconds (default: 30)
  -a, --annotate SYM     perf annotate for this exact symbol
  -o, --output-dir DIR   Profile output root for this run
  -l, --label NAME       Manifest label (e.g. before, after, baseline)
  --compare A B          Diff two profile dirs' metrics.txt (ratios + % delta)
  --preflight            Print host requirements and exit
  --install-tools        Install missing user-space Cargo tools (flamegraph/samply/rustfilt)
  --no-install           Do not auto-install Cargo tools
  --with LIST            Extra tools, comma-separated:
                           flamegraph (default on)
                           samply
                           callgrind  (owner-local phases; requires valgrind)
                           none       disable extras
  --skip LIST            Skip stages, comma-separated:
                           build,stat,record,report,flamegraph,samply,callgrind,annotate,summary
  -h, --help             Show this help

Environment:
  RUNIC_PROFILE_DIR            Output root (default: <repo>/target/runic-profiles)
  RUNIC_PROFILE_FREQ           perf sample frequency (default: 997)
  RUNIC_PROFILE_STAT_REPEATS   perf stat -r (default: 5)
  RUNIC_PROFILE_STAT_SECONDS   Criterion window for perf stat (default: 5)
  RUNIC_PROFILE_CPUS           taskset CPU list
  RUNIC_PROFILE_BIN            Bench ELF when using --skip build
  RUNIC_PROFILE_EVENT          Preferred record event (default: cycles:u)
  RUNIC_PROFILE_FALLBACK_EVENT Fallback record event (default: cpu-clock:u)
  CARGO_PROFILE_BENCH_DEBUG    Passed through (default: line-tables-only)
  RUSTFLAGS                    Frame pointers and v0 symbols forced on if missing

Prerequisites (system; never auto-sudo):
  sudo pacman -S perf util-linux python
  # optional Callgrind:
  sudo pacman -S valgrind

User-space tools (auto-installed when selected, or via --install-tools):
  cargo install --locked flamegraph samply rustfilt

Examples:
  scripts/profile.sh --preflight
  scripts/profile.sh explicit 'explicit/single_size_churn/runic/64'
  scripts/profile.sh -l baseline -t 20 \
    threaded 'threaded/persistent_remote_fan_in/runic/4/live:256'
  scripts/profile.sh --with flamegraph,samply,callgrind \
    explicit 'explicit/owner_free_only/runic/64'
  scripts/profile.sh --compare \
    target/runic-profiles/foo-before target/runic-profiles/foo-after

After a run, inspect Where:
  perf report -i <out>/perf.data
  perf annotate -i <out>/perf.data --symbol '<symbol from report>'
  samply load <out>/samply.json
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

normalize_csv() {
  printf '%s' "$1" | tr '[:upper:]' '[:lower:]' | tr -s ' ,' ',' | sed 's/^,//;s/,$//'
}

# ---------------------------------------------------------------------------
# tool bootstrap
# ---------------------------------------------------------------------------

print_privileged_setup() {
  cat <<'EOF'
Runic profiling host requirements
=================================

Required system packages (Arch):
  sudo pacman -S perf util-linux python

Optional Callgrind:
  sudo pacman -S valgrind

perf counter access (if `perf stat -e cycles:u -- sleep 0.1` fails):
  echo 'kernel.perf_event_paranoid = 1' | sudo tee /etc/sysctl.d/99-perf.conf
  sudo sysctl --system

User-space Cargo tools (no sudo):
  cargo install --locked flamegraph samply rustfilt

Verify:
  perf stat -e cycles:u -- sleep 0.1
  taskset -c 0 true
EOF
}

cargo_bin_dir() {
  if [[ -n ${CARGO_HOME:-} ]]; then
    printf '%s/bin\n' "$CARGO_HOME"
  else
    printf '%s/.cargo/bin\n' "$HOME"
  fi
}

ensure_path_has_cargo_bin() {
  local cargo_bin
  cargo_bin=$(cargo_bin_dir)
  case ":$PATH:" in
    *":$cargo_bin:"*) ;;
    *) export PATH="$cargo_bin:$PATH" ;;
  esac
}

ensure_cargo_tool() {
  local name=$1
  local crate=${2:-$1}
  have_cmd "$name" && return 0
  [[ $INSTALL_TOOLS == yes ]] || {
    warn "$name not found; install with: cargo install --locked $crate"
    return 1
  }
  info "Installing $crate into $(cargo_bin_dir)..."
  cargo install --locked "$crate"
  have_cmd "$name" || die "installed $crate but $name is still not on PATH"
}

ensure_optional_tools() {
  ensure_path_has_cargo_bin
  if want_tool flamegraph; then
    ensure_cargo_tool flamegraph || true
  fi
  if want_tool samply; then
    ensure_cargo_tool samply || true
  fi
  if [[ $INSTALL_TOOLS == yes ]] || want_tool flamegraph || want_tool samply; then
    ensure_cargo_tool rustfilt || true
  fi
  if want_tool callgrind && ! have_cmd valgrind; then
    die "callgrind requested but valgrind is missing; install with: sudo pacman -S valgrind"
  fi
}

check_perf_privilege() {
  local paranoid
  paranoid=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || printf 'unknown')
  info "kernel.perf_event_paranoid=$paranoid"
  if perf stat -e cycles:u -- sleep 0.01 >/dev/null 2>&1; then
    return 0
  fi
  print_privileged_setup >&2
  die "perf cannot read cycles:u; fix privilege settings above and retry"
}

require_system_tools() {
  local missing=()
  have_cmd cargo || missing+=('cargo (rustup)')
  have_cmd rustc || missing+=('rustc (rustup)')
  have_cmd git || missing+=('git')
  have_cmd python3 || missing+=('python (sudo pacman -S python)')
  have_cmd taskset || missing+=('taskset (sudo pacman -S util-linux)')
  have_cmd perf || missing+=('perf (sudo pacman -S perf)')
  if ((${#missing[@]})); then
    print_privileged_setup >&2
    die "missing required tools: ${missing[*]}"
  fi
}

# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

PROFILE_SECONDS=30
HOT_SYMBOL=
OUTPUT_ROOT_OVERRIDE=
RUN_LABEL=
WITH_TOOLS=flamegraph
SKIP_STAGES=
COMPARE_BEFORE=
COMPARE_AFTER=
PREFLIGHT=no
INSTALL_TOOLS=auto

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
    --preflight)
      PREFLIGHT=yes
      shift
      ;;
    --install-tools)
      INSTALL_TOOLS=yes
      shift
      ;;
    --no-install)
      INSTALL_TOOLS=no
      shift
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

# ---------------------------------------------------------------------------
# compare mode (Cost only)
# ---------------------------------------------------------------------------

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

for key in ("bench_target", "criterion_filter"):
    if manifest_b.get(key) and manifest_a.get(key) and manifest_b[key] != manifest_a[key]:
        print(f"warning: {key} differs: {manifest_b[key]!r} vs {manifest_a[key]!r}", file=sys.stderr)

keys = [
    ("cycles_per_elem", "lower"),
    ("instructions_per_elem", "lower"),
    ("branches_per_elem", "lower"),
    ("branch_misses_per_elem", "lower"),
    ("cache_misses_per_elem", "lower"),
    ("elems_per_sec", "higher"),
    ("ipc", "higher"),
    ("mean_reuse_ns", "lower"),
    ("cycles", "lower"),
    ("instructions", "lower"),
    ("seconds", "lower"),
]

print("Runic profile compare")
print("=====================")
print(f"before: {before_dir}")
print(f"  label={manifest_b.get('label', '')} sha={manifest_b.get('git_sha', '')} filter={manifest_b.get('criterion_filter', '')}")
print(f"after:  {after_dir}")
print(f"  label={manifest_a.get('label', '')} sha={manifest_a.get('git_sha', '')} filter={manifest_a.get('criterion_filter', '')}")
print()
print(f"{'metric':<28} {'before':>14} {'after':>14} {'ratio':>10} {'delta%':>10} {'want':>8}")
print("-" * 90)

for key, direction in keys:
    if key not in before or key not in after:
        continue
    try:
        b = float(before[key])
        a = float(after[key])
    except ValueError:
        continue
    if b == 0:
        continue
    ratio = a / b
    delta = (a - b) / b * 100.0
    print(f"{key:<28} {b:14.4f} {a:14.4f} {ratio:10.4f} {delta:9.2f}% {direction:>8}")
PY
}

if [[ -n $COMPARE_BEFORE ]]; then
  [[ $# -eq 0 ]] || die "unexpected arguments after --compare: $*"
  compare_metrics "$COMPARE_BEFORE" "$COMPARE_AFTER"
  exit 0
fi

if [[ $PREFLIGHT == yes ]]; then
  print_privileged_setup
  ensure_path_has_cargo_bin
  info ""
  info "PATH cargo bin: $(cargo_bin_dir)"
  for cmd in cargo rustc git python3 perf taskset flamegraph samply rustfilt valgrind; do
    if have_cmd "$cmd"; then
      info "  present: $cmd ($(command -v "$cmd"))"
    else
      info "  missing: $cmd"
    fi
  done
  if have_cmd perf; then
    check_perf_privilege || true
  fi
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

case ",${WITH_TOOLS}," in
  *,none,*) WITH_TOOLS= ;;
esac

if [[ $INSTALL_TOOLS == auto ]]; then
  INSTALL_TOOLS=yes
fi

# ---------------------------------------------------------------------------
# config / env
# ---------------------------------------------------------------------------

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
cd "$REPO_ROOT"

OUTPUT_ROOT=${OUTPUT_ROOT_OVERRIDE:-${RUNIC_PROFILE_DIR:-$REPO_ROOT/target/runic-profiles}}
SAMPLE_FREQ=${RUNIC_PROFILE_FREQ:-997}
STAT_REPEATS=${RUNIC_PROFILE_STAT_REPEATS:-5}
STAT_PROFILE_SECONDS=${RUNIC_PROFILE_STAT_SECONDS:-5}
RECORD_EVENT=${RUNIC_PROFILE_EVENT:-cycles:u}
FALLBACK_EVENT=${RUNIC_PROFILE_FALLBACK_EVENT:-cpu-clock:u}

[[ $SAMPLE_FREQ =~ ^[0-9]+$ ]] || die "RUNIC_PROFILE_FREQ must be an integer"
[[ $STAT_REPEATS =~ ^[0-9]+$ ]] || die "RUNIC_PROFILE_STAT_REPEATS must be an integer"
[[ $STAT_PROFILE_SECONDS =~ ^[0-9]+$ ]] || die "RUNIC_PROFILE_STAT_SECONDS must be an integer"

require_system_tools
ensure_path_has_cargo_bin
ensure_optional_tools
check_perf_privilege

# Non-multiplexed counter groups: each group is a separate Criterion run under perf.
STAT_GROUPS=(
  'core|task-clock,cycles:u,instructions:u'
  'branch|branches:u,branch-misses:u'
  'cache|cache-references:u,cache-misses:u'
  'dtlb|dTLB-loads:u,dTLB-load-misses:u'
  'l1d|L1-dcache-loads:u,L1-dcache-load-misses:u'
)

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
case " ${RUSTFLAGS:-} " in
  *' -C symbol-mangling-version=v0 '*) ;;
  *) export RUSTFLAGS="$RUSTFLAGS -C symbol-mangling-version=v0" ;;
esac

ALLOWED_CPUS=$(awk '/^Cpus_allowed_list:/ { print $2 }' /proc/self/status)
[[ -n $ALLOWED_CPUS ]] || die "cannot determine profiling CPU affinity"
if [[ -n ${RUNIC_PROFILE_CPUS:-} ]]; then
  PROFILE_CPUS=$RUNIC_PROFILE_CPUS
elif [[ $BENCH_TARGET == *threaded* ]]; then
  PROFILE_CPUS=$ALLOWED_CPUS
else
  PROFILE_CPUS=${ALLOWED_CPUS%%,*}
  PROFILE_CPUS=${PROFILE_CPUS%%-*}
fi

GIT_SHA=$(git rev-parse --short=12 HEAD 2>/dev/null || printf 'unknown')
if git diff --quiet --ignore-submodules -- 2>/dev/null && git diff --cached --quiet --ignore-submodules -- 2>/dev/null; then
  GIT_DIRTY=no
else
  GIT_DIRTY=yes
fi

if [[ $CRITERION_FILTER == *setup_lifecycle* ]]; then
  warn "setup_lifecycle filters intentionally include spawn/join/unbind noise"
elif [[ $BENCH_TARGET == *threaded* && $CRITERION_FILTER != *persistent_* ]]; then
  warn "non-persistent threaded filters may include thread-setup noise; prefer persistent_*"
fi

if want_tool callgrind && [[ $BENCH_TARGET == *threaded* || $CRITERION_FILTER == *remote* || $CRITERION_FILTER == *fan_in* || $CRITERION_FILTER == *free_ring* ]]; then
  warn "Callgrind is for owner-local phases only; concurrent/remote filters are not meaningful under simulation"
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

# ---------------------------------------------------------------------------
# pipeline helpers
# ---------------------------------------------------------------------------

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
fresh = [c for is_fresh, c in candidates if is_fresh]
pool = fresh or [c for _, c in candidates]
pool = [c for c in pool if os.path.isfile(c)]
if not pool:
    sys.exit(1)
print(max(pool, key=lambda p: os.path.getmtime(p)))
PY
  ) || die "failed to resolve bench executable for $target (see $json_log / ${json_log}.stderr)"

  [[ -x $bin ]] || die "resolved bench binary is not executable: $bin"
  printf '%s\n' "$bin"
}

run_perf_stat() {
  local output_file=$1
  shift
  : >"$output_file"

  local entry group events
  for entry in "${STAT_GROUPS[@]}"; do
    group=${entry%%|*}
    events=${entry#*|}
    {
      printf '# group=%s events=%s\n' "$group" "$events"
      printf '# command: '
      print_command perf stat -r "$STAT_REPEATS" -e "$events" -- "$@"
      printf '\n'
    } >>"$output_file"

    if ! perf stat -r "$STAT_REPEATS" -e "$events" -- "$@" >>"$output_file" 2>&1; then
      die "perf stat failed for group=$group"
    fi
    printf '\n' >>"$output_file"
  done
}

record_profile() {
  local output_file=$1
  local event_file=$2
  shift 2

  local record_log="${output_file}.log"
  local event
  : >"$record_log"

  for event in "$RECORD_EVENT" "$FALLBACK_EVENT"; do
    rm -f "$output_file"
    {
      printf '# perf record event=%s\n' "$event"
      printf '# command: '
      print_command perf record -F "$SAMPLE_FREQ" -e "$event" -b -g --call-graph fp \
        -o "$output_file" -- "$@"
      printf '\n'
    } >>"$record_log"

    if perf record -F "$SAMPLE_FREQ" -e "$event" -b -g --call-graph fp \
      -o "$output_file" -- "$@" >>"$record_log" 2>&1; then
      printf '%s\n' "$event" >"$event_file"
      return 0
    fi
    warn "perf record with $event failed"
  done

  die "perf record failed for preferred and fallback events"
}

write_callgrind() {
  local log=$1
  shift

  have_cmd valgrind || {
    warn "valgrind not installed; skip callgrind"
    return 0
  }

  {
    printf '# callgrind (owner-local deterministic instruction counts)\n'
    printf '# command: '
    print_command valgrind --tool=callgrind --callgrind-out-file="$OUT_DIR/callgrind.out" \
      --branch-sim=yes --cache-sim=yes -- "$@"
    printf '\n'
  } >"$log"

  if valgrind --tool=callgrind --callgrind-out-file="$OUT_DIR/callgrind.out" \
    --branch-sim=yes --cache-sim=yes -- "$@" >>"$log" 2>&1; then
    info "Wrote $OUT_DIR/callgrind.out"
  else
    warn "callgrind failed; see $log"
  fi
}

write_flamegraph() {
  local perf_data=$1
  local svg=$2
  local log=$3

  have_cmd flamegraph || {
    warn "flamegraph not installed; skip SVG"
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
    warn "samply not installed; skip"
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

  warn "samply import failed; see $log"
}

write_metrics() {
  local output_file=$1
  python3 - "$OUT_DIR/perf-stat.txt" "$output_file" <<'PY'
import re
import sys

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
branch_misses = first_number(r"([\d,]+)\s+branch-misses:u")
cache_refs = first_number(r"([\d,]+)\s+cache-references:u")
cache_misses = first_number(r"([\d,]+)\s+cache-misses:u")
dtlb_loads = first_number(r"([\d,]+)\s+dTLB-loads:u")
dtlb_misses = first_number(r"([\d,]+)\s+dTLB-load-misses:u")
l1d_loads = first_number(r"([\d,]+)\s+L1-dcache-loads:u")
l1d_misses = first_number(r"([\d,]+)\s+L1-dcache-load-misses:u")
task = first_number(r"([\d,.]+)\s+msec task-clock")

triples = re.findall(
    r"thrpt:\s*\[([\d.]+)\s*([KMG]?)elem/s\s+([\d.]+)\s*([KMG]?)elem/s\s+([\d.]+)\s*([KMG]?)elem/s\]",
    text,
)
scale = {"": 1.0, "K": 1e3, "M": 1e6, "G": 1e9}
elems_per_s = None
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

cycle_rows = [
    float(v.replace(",", ""))
    for v in re.findall(r"^#\s+([\d,]+)\s+cycles:u", text, flags=re.M)
]
spread = None
if cycle_rows and min(cycle_rows) > 0:
    mid = sorted(cycle_rows)[len(cycle_rows) // 2]
    spread = (max(cycle_rows) - min(cycle_rows)) / mid * 100.0

lines = []
if cycles is not None:
    lines.append(f"cycles={cycles:.0f}")
if insns is not None:
    lines.append(f"instructions={insns:.0f}")
if branches is not None:
    lines.append(f"branches={branches:.0f}")
if branch_misses is not None:
    lines.append(f"branch_misses={branch_misses:.0f}")
if cache_refs is not None:
    lines.append(f"cache_references={cache_refs:.0f}")
if cache_misses is not None:
    lines.append(f"cache_misses={cache_misses:.0f}")
if dtlb_loads is not None:
    lines.append(f"dtlb_loads={dtlb_loads:.0f}")
if dtlb_misses is not None:
    lines.append(f"dtlb_load_misses={dtlb_misses:.0f}")
if l1d_loads is not None:
    lines.append(f"l1d_loads={l1d_loads:.0f}")
if l1d_misses is not None:
    lines.append(f"l1d_load_misses={l1d_misses:.0f}")
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
            if branch_misses is not None:
                lines.append(f"branch_misses_per_elem={branch_misses / total_elems:.6f}")
            if cache_misses is not None:
                lines.append(f"cache_misses_per_elem={cache_misses / total_elems:.6f}")
            if dtlb_misses is not None:
                lines.append(f"dtlb_load_misses_per_elem={dtlb_misses / total_elems:.6f}")
            if l1d_misses is not None:
                lines.append(f"l1d_load_misses_per_elem={l1d_misses / total_elems:.6f}")
if cycles is not None and insns is not None and cycles > 0:
    lines.append(f"ipc={insns / cycles:.4f}")
if mean_reuse is not None:
    lines.append(f"mean_reuse_ns={mean_reuse:.0f}")
if spread is not None:
    lines.append(f"cycles_spread_pct={spread:.3f}")

open(out_path, "w", encoding="utf-8").write("\n".join(lines) + ("\n" if lines else ""))

if cycles is None or insns is None:
    raise SystemExit("missing core counters (cycles/instructions) in perf-stat.txt")
if elems_per_s is None:
    raise SystemExit("missing Criterion thrpt in perf-stat.txt")
if spread is not None and spread > 3.0:
    raise SystemExit(f"cycle spread {spread:.2f}% exceeds 3%")
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
    printf 'cpus:    %s\n' "$PROFILE_CPUS"
    printf 'git:     %s dirty=%s\n' "$GIT_SHA" "$GIT_DIRTY"
    printf 'out:     %s\n' "$OUT_DIR"
    printf '\n'

    if [[ -f $OUT_DIR/metrics.txt ]]; then
      printf '%s\n' '--- Cost (metrics.txt) ---'
      cat "$OUT_DIR/metrics.txt"
      printf '\n'
    fi

    if [[ -f $OUT_DIR/perf-stat.txt ]]; then
      printf '%s\n' '--- perf stat (tail) ---'
      tail -n 40 "$OUT_DIR/perf-stat.txt"
      printf '\n'
    fi

    if [[ -f $OUT_DIR/perf-report-flat.txt ]]; then
      printf '%s\n' '--- Where: top flat symbols ---'
      awk '
        BEGIN { n = 0 }
        /^#/ { if ($0 ~ /Overhead|Samples|Event/) print; next }
        /^$/ { next }
        {
          print
          if (++n >= 25) exit
        }
      ' "$OUT_DIR/perf-report-flat.txt"
      printf '\n'
    fi

    printf '%s\n' 'Where next:'
    printf '  perf report -i %s/perf.data\n' "$OUT_DIR"
    printf '  perf annotate -i %s/perf.data --symbol '\''<from report>'\''\n' "$OUT_DIR"
    if [[ -f $OUT_DIR/samply.json ]]; then
      printf '  samply load %s/samply.json\n' "$OUT_DIR"
    fi
    printf '\n'

    printf '%s\n' 'Artifacts:'
    for f in metadata.txt manifest.txt command.txt metrics.txt \
      perf-stat.txt perf.data \
      perf-report-flat.txt perf-report-self.txt perf-report-children.txt \
      flamegraph.svg flamegraph.log samply.json samply.log callgrind.out callgrind.log \
      perf-annotate.txt summary.txt; do
      if [[ -e $OUT_DIR/$f ]]; then
        printf '  %s\n' "$f"
      fi
    done
  } >"$tmp"

  umask 077
  cat "$tmp" >"$output_file"
  rm -f "$tmp"
  cat "$output_file"
}

write_metadata() {
  local output_file=$1
  local event_used=$2
  {
    printf 'git_sha=%s\n' "$GIT_SHA"
    printf 'git_dirty=%s\n' "$GIT_DIRTY"
    printf 'rustc=%s\n' "$(rustc -Vv | awk '/^release:/ { print $2; exit }')"
    printf 'host=%s\n' "$(uname -a)"
    printf 'bench_target=%s\n' "$BENCH_TARGET"
    printf 'criterion_filter=%s\n' "$CRITERION_FILTER"
    printf 'label=%s\n' "${RUN_LABEL:-}"
    printf 'profile_seconds=%s\n' "$PROFILE_SECONDS"
    printf 'stat_seconds=%s\n' "$STAT_PROFILE_SECONDS"
    printf 'stat_repeats=%s\n' "$STAT_REPEATS"
    printf 'sample_freq=%s\n' "$SAMPLE_FREQ"
    printf 'record_event=%s\n' "$event_used"
    printf 'cpus=%s\n' "$PROFILE_CPUS"
    printf 'cargo_profile_bench_debug=%s\n' "${CARGO_PROFILE_BENCH_DEBUG:-}"
    printf 'rustflags=%s\n' "${RUSTFLAGS:-}"
    printf 'with_tools=%s\n' "$WITH_TOOLS"
    printf 'skip_stages=%s\n' "${SKIP_STAGES:-}"
    printf 'bench_bin=%s\n' "${BENCH_BIN:-}"
  } >"$output_file"
}

write_manifest() {
  local output_file=$1
  local event_used=$2
  {
    printf 'label=%s\n' "${RUN_LABEL:-}"
    printf 'git_sha=%s\n' "$GIT_SHA"
    printf 'git_dirty=%s\n' "$GIT_DIRTY"
    printf 'bench_target=%s\n' "$BENCH_TARGET"
    printf 'criterion_filter=%s\n' "$CRITERION_FILTER"
    printf 'event=%s\n' "$event_used"
    printf 'out_dir=%s\n' "$OUT_DIR"
  } >"$output_file"
}

# ---------------------------------------------------------------------------
# pipeline
# ---------------------------------------------------------------------------

PROFILE_ARGS=(
  "$CRITERION_FILTER"
  --exact
  --profile-time "$PROFILE_SECONDS"
  --warm-up-time 0.5
  --noplot
  --bench
)

STAT_ARGS=(
  "$CRITERION_FILTER"
  --exact
  --warm-up-time 0.25
  --measurement-time "$STAT_PROFILE_SECONDS"
  --sample-size 10
  --noplot
  --bench
)

BENCH_BIN=
EVENT_USED=$RECORD_EVENT

if want_stage build; then
  info "Resolving optimized bench binary..."
  BENCH_BIN=$(resolve_bench_bin "$BENCH_TARGET" "$OUT_DIR/cargo-build.json")
elif [[ -n ${RUNIC_PROFILE_BIN:-} ]]; then
  BENCH_BIN=$RUNIC_PROFILE_BIN
  [[ -x $BENCH_BIN ]] || die "RUNIC_PROFILE_BIN is not executable: $BENCH_BIN"
else
  die "build skipped; set RUNIC_PROFILE_BIN to the resolved bench ELF"
fi
info "Bench binary: $BENCH_BIN"

{
  printf 'bench_bin=%s\n' "$BENCH_BIN"
  printf 'stat: '
  print_command taskset -c "$PROFILE_CPUS" perf stat ... -- "$BENCH_BIN" "${STAT_ARGS[@]}"
  printf 'record: '
  print_command taskset -c "$PROFILE_CPUS" perf record ... -- "$BENCH_BIN" "${PROFILE_ARGS[@]}"
} >"$OUT_DIR/command.txt"

if want_stage stat; then
  info "Running perf stat on resolved binary..."
  run_perf_stat "$OUT_DIR/perf-stat.txt" \
    taskset -c "$PROFILE_CPUS" "$BENCH_BIN" "${STAT_ARGS[@]}"
  write_metrics "$OUT_DIR/metrics.txt"
fi

if want_stage record; then
  info "Recording profile (${PROFILE_SECONDS}s) on resolved binary..."
  record_profile "$OUT_DIR/perf.data" "$OUT_DIR/event-used.txt" \
    taskset -c "$PROFILE_CPUS" "$BENCH_BIN" "${PROFILE_ARGS[@]}"
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

if want_stage callgrind && want_tool callgrind; then
  info "Running Callgrind..."
  write_callgrind "$OUT_DIR/callgrind.log" \
    taskset -c "$PROFILE_CPUS" "$BENCH_BIN" "${PROFILE_ARGS[@]}"
fi

if want_stage annotate && [[ -n $HOT_SYMBOL ]]; then
  [[ -f $OUT_DIR/perf.data ]] || die "missing perf.data; cannot annotate"
  info "Annotating $HOT_SYMBOL..."
  if ! perf annotate --stdio -i "$OUT_DIR/perf.data" --symbol "$HOT_SYMBOL" \
    >"$OUT_DIR/perf-annotate.txt" 2>"$OUT_DIR/perf-annotate.err"; then
    warn "perf annotate failed for symbol: $HOT_SYMBOL (see perf-annotate.err)"
  else
    info "Wrote $OUT_DIR/perf-annotate.txt"
  fi
fi

write_metadata "$OUT_DIR/metadata.txt" "$EVENT_USED"
write_manifest "$OUT_DIR/manifest.txt" "$EVENT_USED"

if want_stage summary; then
  info ""
  info "======== summary ========"
  write_summary "$OUT_DIR/summary.txt" "$EVENT_USED"
  info "========================="
fi

info "Profile artifacts written to $OUT_DIR"
