#!/usr/bin/env bash
#
# reap-agents.sh — find and kill orphaned/zombie code-agent processes that
# stella spawned and left behind.
#
#   Usage:  scripts/reap-agents.sh [--yes] [--dry-run] [--verbose]
#                                  [--min-idle-secs N] [--sample-secs N]
#
#     --yes / -y        skip the confirmation prompt and kill immediately
#     --dry-run         only report candidates, never send a signal
#     --verbose / -v    explain why each orphan did or didn't qualify
#     --min-idle-secs   age threshold before a process is even considered
#                       (default 1200 = 20 minutes)
#     --sample-secs     CPU-activity sampling window (default 5)
#
# Two things this hunts, both left behind the same way: a hard kill of the
# parent (crash, OOM, closed terminal) reparents its children to launchd
# (ppid 1) without ever running the cleanup that would normally catch them.
#
#   (A) An orphaned `stella`/`stella-dev` process itself — the code agent
#       proper, still running (still spending, still holding tool state)
#       with nothing left attached to it. Identified by binary name alone;
#       no cwd or command-line guessing needed.
#
#   (B) An orphaned OS subprocess a *now-dead* stella spawned mid-turn — the
#       bash tool, a custom tool, the hook runner, ocp-host's stdio
#       provider connections. Each calls setsid() before exec and carries
#       its own Drop-based "kill_group" backstop (ocp-host/src/stdio.rs,
#       stella-tools/src/bash.rs) — but that backstop is Rust destructor
#       logic, which a SIGKILL'd parent never runs.
#
#       Post-hoc, exact attribution for (B) isn't fully recoverable: a
#       single simple command (`bash -c "sleep 600"`) gets bash's
#       exec-optimization and *becomes* `sleep` in place, so matching on
#       comm/argv is unreliable. What survives is the process's cwd, which
#       stella always sets to the workspace/worktree root
#       (stella-tools/src/bash.rs, stella-fleet/src/git.rs). So (B) is
#       identified as: a process-group leader (pid == pgid — true of every
#       stella-spawned child, though also true of plenty of ordinary
#       backgrounded daemons, so this narrows but doesn't uniquely
#       fingerprint stella) whose cwd sits inside a stella-managed
#       workspace — a directory with its own `.stella/`, or one climbing to
#       such a directory before hitting a `.git` boundary or $HOME,
#       or a `.stella/worktrees/<slug>/` isolated fleet task dir
#       (stella-fleet/src/git.rs). This is circumstantial, not proof — the
#       report always prints the full command and cwd, and killing always
#       goes through the confirmation prompt below, so an unrelated
#       process that happens to share a cwd gets a chance to be vetoed.
#
# "No activity in the last 20 minutes" is approximated the only way a
# single-shot script can: the process must be at least --min-idle-secs old
# (default 20m — comfortably past every internal timeout stella's own
# tools enforce while a parent is alive, so surviving that long already
# implies the parent is gone) AND its cumulative CPU time must not move
# across a live --sample-secs sample taken just now. That is "old and
# idle right now", not a continuously-monitored 20-minute idle window — a
# single-shot script has no history to check that against. A real orphan
# still burning CPU (a genuine long build) is left alone either way.
#
# Zombies (state Z) can't be signalled — only their parent's wait() clears
# them. For an orphan that parent is launchd, which reaps zombies on its
# own; they are reported, never signalled.

set -uo pipefail

MIN_IDLE_SECS=1200
SAMPLE_SECS=5
ASSUME_YES=0
DRY_RUN=0
VERBOSE=0

usage() {
  sed -n '2,15p' "$0"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --yes|-y) ASSUME_YES=1 ;;
    --dry-run) DRY_RUN=1 ;;
    --verbose|-v) VERBOSE=1 ;;
    --min-idle-secs) MIN_IDLE_SECS="$2"; shift ;;
    --sample-secs) SAMPLE_SECS="$2"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 1 ;;
  esac
  shift
done

vlog() { [[ "$VERBOSE" -eq 1 ]] && echo "  · $*" >&2; return 0; }

# Parses ps's `[[dd-]hh:]mm:ss` elapsed-time format into total seconds.
# Every component is forced to base 10 (`10#`) — bash's arithmetic context
# otherwise reads a leading-zero field like "09" as an invalid octal digit.
etime_to_secs() {
  local etime="$1" days=0 rest="$1" hh=0 mm=0 ss=0 a="" b="" c=""
  if [[ "$etime" == *-* ]]; then
    days="${etime%%-*}"
    rest="${etime#*-}"
  fi
  IFS=: read -r a b c <<<"$rest"
  if [[ -n "$c" ]]; then
    hh=$a; mm=$b; ss=$c
  elif [[ -n "$b" ]]; then
    mm=$a; ss=$b
  else
    ss=$a
  fi
  echo $(( 10#$days*86400 + 10#$hh*3600 + 10#$mm*60 + 10#$ss ))
}

# Resolves a pid's current working directory. lsof ships on macOS; Linux
# falls back to the /proc cwd symlink lsof would otherwise read anyway.
proc_cwd() {
  local pid="$1"
  if command -v lsof >/dev/null 2>&1; then
    lsof -a -p "$pid" -d cwd -Fn 2>/dev/null | awk '/^n/{print substr($0,2); exit}'
  elif [[ -r "/proc/$pid/cwd" ]]; then
    readlink -f "/proc/$pid/cwd" 2>/dev/null
  fi
}

# True if `dir` is a stella-managed workspace: it has a `.stella/` dir
# itself (a `run`/`chat`/shared-tree-fleet workspace root), one of its
# ancestors up to the nearest repo root does (a tool command that `cd`'d
# into a subdirectory before hanging), or it sits under a
# `.stella/worktrees/<slug>/` isolated fleet task.
#
# The climb stops at the first `.git` boundary (repo root) or at $HOME,
# whichever comes first — never higher. A stray *global* `~/.stella` (a
# real thing: running stella directly from $HOME creates one) would
# otherwise match nearly every process on the box, since every path
# eventually climbs through $HOME.
is_stella_workspace() {
  local dir="$1"
  [[ -z "$dir" ]] && return 1
  case "$dir" in
    */.stella/worktrees/*) return 0 ;;
  esac
  while [[ -n "$dir" && "$dir" != "/" && "$dir" != "$HOME" ]]; do
    [[ -d "$dir/.stella" ]] && return 0
    [[ -e "$dir/.git" ]] && return 1 # repo root reached without a match
    dir="$(dirname "$dir")"
  done
  return 1
}

UNAME="$(uname -s)"
if [[ "$UNAME" == "Darwin" ]]; then
  PS_SNAPSHOT=(ps -Awwo "pid=,ppid=,pgid=,stat=,etime=,time=,comm=")
else
  PS_SNAPSHOT=(env COLUMNS=2000 ps -eo "pid=,ppid=,pgid=,stat=,etime=,time=,comm=")
fi

resample_cputime() {
  "${PS_SNAPSHOT[@]}" 2>/dev/null | awk -v p="$1" '$1==p{print $6; exit}'
}

echo "Scanning for orphaned stella code agents and tool subprocesses (idle >= ${MIN_IDLE_SECS}s)..."

declare -a kill_pids=()
declare -a kill_labels=()
zombie_count=0
checked=0

# `while read < <(...)` rather than `mapfile` — this must run under macOS's
# stock /bin/bash (3.2, no `mapfile`/`readarray` builtin), not just bash 4+.
while IFS= read -r row; do
  [[ -z "$row" ]] && continue
  # shellcheck disable=SC2206
  fields=($row)
  pid="${fields[0]:-}"
  ppid="${fields[1]:-}"
  pgid="${fields[2]:-}"
  stat="${fields[3]:-}"
  etime="${fields[4]:-}"
  cputime="${fields[5]:-}"
  comm="${fields[6]:-}"
  [[ -z "$pid" ]] && continue

  category=""
  case "${comm##*/}" in
    stella|stella-dev) category="agent" ;;
    *)
      if [[ "$pid" == "$pgid" ]]; then
        cwd="$(proc_cwd "$pid")"
        is_stella_workspace "$cwd" && category="subprocess"
      fi
      ;;
  esac
  [[ -z "$category" ]] && continue
  [[ "$category" == "agent" ]] && cwd="$(proc_cwd "$pid")"
  checked=$((checked + 1))

  if [[ "$stat" == *Z* ]]; then
    zombie_count=$((zombie_count + 1))
    echo "  zombie (defunct, awaiting launchd reap): pid=$pid comm=$comm cwd=${cwd:-?}"
    continue
  fi

  age=$(etime_to_secs "$etime")
  if (( age < MIN_IDLE_SECS )); then
    vlog "pid=$pid ($comm, $category) too young: ${age}s < ${MIN_IDLE_SECS}s — left alone"
    continue
  fi

  cputime_before="$cputime"
  sleep "$SAMPLE_SECS"
  cputime_after="$(resample_cputime "$pid")"
  if [[ -z "$cputime_after" ]]; then
    vlog "pid=$pid ($comm, $category) vanished during sampling — left alone"
    continue
  fi
  if [[ "$cputime_after" != "$cputime_before" ]]; then
    vlog "pid=$pid ($comm, $category) still burning CPU ($cputime_before -> $cputime_after) — left alone, not idle"
    continue
  fi

  full_cmd="$(ps -o command= -p "$pid" 2>/dev/null)"
  label="[$category] pid=$pid  ppid=$ppid  age=${etime}  cwd=${cwd:-?}  cmd=${full_cmd:-$comm}"
  kill_pids+=("$pid")
  kill_labels+=("$label")
done < <("${PS_SNAPSHOT[@]}" 2>/dev/null | awk '$2==1')

echo
echo "Checked $checked orphaned candidate(s); $zombie_count zombie(s) seen (not signalled, launchd reaps those)."

if [[ ${#kill_pids[@]} -eq 0 ]]; then
  echo "No idle orphaned code agents found. Nothing to kill."
  exit 0
fi

echo "Idle orphans (no CPU activity across a ${SAMPLE_SECS}s sample, age >= ${MIN_IDLE_SECS}s):"
echo "  [agent]      an orphaned stella/stella-dev process itself"
echo "  [subprocess] an orphaned tool-call child of a now-dead stella (bash/custom-tool/etc.)"
for label in "${kill_labels[@]}"; do
  echo "  - $label"
done

if [[ "$DRY_RUN" -eq 1 ]]; then
  echo
  echo "(--dry-run: not killing anything)"
  exit 0
fi

if [[ "$ASSUME_YES" -ne 1 ]]; then
  echo
  read -r -p "Kill these ${#kill_pids[@]} process group(s)? [y/N] " reply
  case "$reply" in
    y|Y|yes|YES) ;;
    *) echo "Aborted — nothing killed."; exit 0 ;;
  esac
fi

echo
for pid in "${kill_pids[@]}"; do
  echo "SIGTERM -> pgid $pid"
  kill -TERM "-$pid" 2>/dev/null
  # Not every candidate is its own group leader (e.g. a lone orphaned
  # `stella` process may not be) — also signal the bare pid as a fallback.
  kill -TERM "$pid" 2>/dev/null
done

sleep 3

for pid in "${kill_pids[@]}"; do
  if kill -0 "$pid" 2>/dev/null; then
    echo "still alive, SIGKILL -> pgid $pid"
    kill -KILL "-$pid" 2>/dev/null
    kill -KILL "$pid" 2>/dev/null
  fi
done

echo "Done — reaped ${#kill_pids[@]} orphaned code-agent process group(s)."
