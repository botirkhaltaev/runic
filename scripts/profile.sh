#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf 'Usage: %s <bench-target> <criterion-filter> [profile-seconds] [hot-symbol]\n' "$0"
  printf '\n'
  printf 'Examples:\n'
  printf '  %s compare_explicit %q\n' "$0" 'compare/explicit/single_size_churn/runic/64'
  printf '  %s compare_explicit %q 30 %q\n' "$0" 'compare/explicit/alloc_zeroed/runic/64' 'runic_core::heap::run::Run::free_local'
  printf '\n'
  printf 'Environment overrides:\n'
  printf '  RUNIC_PROFILE_DIR           output root, default /tmp/opencode/runic-profiles\n'
  printf '  RUNIC_PROFILE_FREQ          perf sample frequency, default 997\n'
  printf '  RUNIC_PROFILE_STAT_REPEATS  perf stat repeats, default 5\n'
  printf '  RUNIC_PROFILE_EVENT         preferred record event, default cycles:u\n'
  printf '  RUNIC_PROFILE_FALLBACK_EVENT fallback record event, default cpu-clock:u\n'
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

warn() {
  printf 'warning: %s\n' "$*" >&2
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

print_command() {
  local arg
  for arg in "$@"; do
    printf '%q ' "$arg"
  done
  printf '\n'
}

run_perf_stat() {
  local output_file=$1
  shift

  local stat_events='cycles:u,instructions:u,branches:u,branch-misses:u,cache-references:u,cache-misses:u'
  {
    printf '# perf stat with user-space hardware counters\n'
    printf '# command: '
    print_command perf stat -r "$STAT_REPEATS" -e "$stat_events" -- "$@"
    printf '\n'
  } >"$output_file"

  if perf stat -r "$STAT_REPEATS" -e "$stat_events" -- "$@" >>"$output_file" 2>&1; then
    return 0
  fi

  warn "perf stat hardware counters failed; retrying with perf stat -d"
  {
    printf '\n# hardware counter stat failed; fallback to perf stat -d\n'
    printf '# command: '
    print_command perf stat -r "$STAT_REPEATS" -d -- "$@"
    printf '\n'
  } >>"$output_file"

  if perf stat -r "$STAT_REPEATS" -d -- "$@" >>"$output_file" 2>&1; then
    return 0
  fi

  warn "perf stat fallback failed; continuing to perf record"
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

write_metadata() {
  local output_file=$1
  local event_used=$2

  {
    printf 'date=%s\n' "$(date -Is)"
    printf 'repo=%s\n' "$REPO_ROOT"
    printf 'branch=%s\n' "$(git rev-parse --abbrev-ref HEAD 2>/dev/null || printf 'unknown')"
    printf 'git_sha=%s\n' "$GIT_SHA"
    printf 'git_dirty=%s\n' "$GIT_DIRTY"
    printf 'bench_target=%s\n' "$BENCH_TARGET"
    printf 'criterion_filter=%s\n' "$CRITERION_FILTER"
    printf 'profile_seconds=%s\n' "$PROFILE_SECONDS"
    printf 'sample_frequency=%s\n' "$SAMPLE_FREQ"
    printf 'stat_repeats=%s\n' "$STAT_REPEATS"
    printf 'record_event=%s\n' "$event_used"
    printf 'cargo_profile_bench_debug=%s\n' "$CARGO_PROFILE_BENCH_DEBUG"
    printf 'rustflags=%s\n' "$RUSTFLAGS"
    printf 'rustc=%s\n' "$(rustc --version)"
    printf 'cargo=%s\n' "$(cargo --version)"
    printf 'perf=%s\n' "$(perf version)"
    printf 'kernel=%s\n' "$(uname -a)"
    printf '\n[git status --short]\n'
    git status --short
  } >"$output_file"
}

if [[ ${1:-} == '-h' || ${1:-} == '--help' ]]; then
  usage
  exit 0
fi

if [[ $# -lt 2 || $# -gt 4 ]]; then
  usage >&2
  exit 2
fi

BENCH_TARGET=$1
CRITERION_FILTER=$2
PROFILE_SECONDS=${3:-30}
HOT_SYMBOL=${4:-}

[[ $PROFILE_SECONDS =~ ^[0-9]+$ ]] || die "profile-seconds must be an integer"

OUTPUT_ROOT=${RUNIC_PROFILE_DIR:-/tmp/opencode/runic-profiles}
SAMPLE_FREQ=${RUNIC_PROFILE_FREQ:-997}
STAT_REPEATS=${RUNIC_PROFILE_STAT_REPEATS:-5}
RECORD_EVENT=${RUNIC_PROFILE_EVENT:-cycles:u}
FALLBACK_EVENT=${RUNIC_PROFILE_FALLBACK_EVENT:-cpu-clock:u}

[[ $SAMPLE_FREQ =~ ^[0-9]+$ ]] || die "RUNIC_PROFILE_FREQ must be an integer"
[[ $STAT_REPEATS =~ ^[0-9]+$ ]] || die "RUNIC_PROFILE_STAT_REPEATS must be an integer"

require_command cargo
require_command rustc
require_command git
require_command perf
require_command uname
require_command date
require_command tr

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
cd "$REPO_ROOT"

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

if [[ $BENCH_TARGET == *threaded* ]]; then
  warn "threaded Criterion benches spawn threads inside iterations; allocator-internal profiles may include thread setup noise"
fi

timestamp=$(date +%Y%m%d-%H%M%S)
slug=$(printf '%s-%s' "$BENCH_TARGET" "$CRITERION_FILTER" | tr -cs '[:alnum:]_.-' '-')
OUT_DIR="$OUTPUT_ROOT/$timestamp-$GIT_SHA-$slug"
mkdir -p "$OUT_DIR"

BUILD_CMD=(cargo bench -p runic-bench --bench "$BENCH_TARGET" --no-run)
STAT_CMD=(cargo bench -p runic-bench --bench "$BENCH_TARGET" -- "$CRITERION_FILTER" --exact --quiet)
PROFILE_CMD=(cargo bench -p runic-bench --bench "$BENCH_TARGET" -- "$CRITERION_FILTER" --exact --profile-time "$PROFILE_SECONDS" --noplot --quiet)

{
  printf '[build]\n'
  print_command "${BUILD_CMD[@]}"
  printf '\n[perf stat]\n'
  print_command perf stat -r "$STAT_REPEATS" -e 'cycles:u,instructions:u,branches:u,branch-misses:u,cache-references:u,cache-misses:u' -- "${STAT_CMD[@]}"
  printf '\n[perf record]\n'
  print_command perf record -F "$SAMPLE_FREQ" -e "$RECORD_EVENT" -g --call-graph fp -o "$OUT_DIR/perf.data" -- "${PROFILE_CMD[@]}"
} >"$OUT_DIR/command.txt"

printf 'Building optimized bench with profiling metadata...\n'
"${BUILD_CMD[@]}"

printf 'Running perf stat...\n'
run_perf_stat "$OUT_DIR/perf-stat.txt" "${STAT_CMD[@]}"

printf 'Recording profile...\n'
record_profile "$OUT_DIR/perf.data" "$OUT_DIR/event-used.txt" "${PROFILE_CMD[@]}"
EVENT_USED=$(<"$OUT_DIR/event-used.txt")

printf 'Generating perf reports...\n'
perf report --stdio --no-children --percent-limit 0.5 -i "$OUT_DIR/perf.data" --sort=dso,symbol >"$OUT_DIR/perf-report-self.txt"
perf report --stdio --children --percent-limit 0.5 -i "$OUT_DIR/perf.data" --sort=dso,symbol >"$OUT_DIR/perf-report-children.txt"

if [[ -n $HOT_SYMBOL ]]; then
  perf annotate --stdio -i "$OUT_DIR/perf.data" --symbol "$HOT_SYMBOL" >"$OUT_DIR/perf-annotate.txt" || warn "perf annotate failed for symbol: $HOT_SYMBOL"
fi

write_metadata "$OUT_DIR/metadata.txt" "$EVENT_USED"

printf 'Profile artifacts written to %s\n' "$OUT_DIR"
