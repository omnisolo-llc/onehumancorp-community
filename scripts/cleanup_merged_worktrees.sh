#!/usr/bin/env bash
set -euo pipefail

# Safely audit and reclaim disk space from secondary git worktrees whose commits
# are already merged into the target branch (default: main).
#
# Safety guarantees (ALL THREE must hold before removing any worktree):
#   1. Working tree is 100% clean (no modified, staged, or untracked files).
#   2. Worktree HEAD is an ancestor of the target branch (merged into main).
#   3. No live OS process has its current working directory inside the worktree.
# Explicitly locked worktrees and the primary checkout are always preserved.

MODE="dry-run"
TARGET_BRANCH="main"
REPO_ARG=""

usage() {
  cat <<'EOF'
Usage: scripts/cleanup_merged_worktrees.sh [OPTIONS]

Options:
  --dry-run          Inspect worktrees and merged branches without changing anything (default).
  --apply            Remove clean, idle, merged secondary worktrees, prune metadata,
                     and delete merged local branches (excluding the target branch).
  --target <branch>  Target branch to verify ancestry against (default: main).
  --repo <path>      Repository path to inspect (default: current git repository).
  -h, --help         Show this help text.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run)
      MODE="dry-run"
      shift
      ;;
    --apply)
      MODE="apply"
      shift
      ;;
    --target)
      if [[ $# -lt 2 || -z "$2" ]]; then
        echo "ERROR: --target requires a branch name" >&2
        exit 2
      fi
      TARGET_BRANCH="$2"
      shift 2
      ;;
    --repo)
      if [[ $# -lt 2 || -z "$2" ]]; then
        echo "ERROR: --repo requires a directory path" >&2
        exit 2
      fi
      REPO_ARG="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "ERROR: unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ -n "$REPO_ARG" ]]; then
  if [[ ! -d "$REPO_ARG" ]]; then
    echo "ERROR: repository path does not exist: $REPO_ARG" >&2
    exit 1
  fi
  START_DIR="$(cd -P "$REPO_ARG" && pwd)"
else
  START_DIR="$(pwd -P)"
fi

if ! git -C "$START_DIR" rev-parse --git-dir >/dev/null 2>&1; then
  echo "ERROR: not inside a git repository: $START_DIR" >&2
  exit 1
fi

CALLER_PWD="$(pwd -P)"

# Identify primary worktree (first worktree entry in `git worktree list --porcelain`).
PRIMARY_ROOT=""
while IFS= read -r line; do
  if [[ "$line" == worktree\ * ]]; then
    PRIMARY_ROOT="${line#worktree }"
    break
  fi
done < <(git -C "$START_DIR" worktree list --porcelain)

if [[ -z "$PRIMARY_ROOT" || ! -d "$PRIMARY_ROOT" ]]; then
  echo "ERROR: unable to resolve primary worktree root" >&2
  exit 1
fi
PRIMARY_ROOT="$(cd -P "$PRIMARY_ROOT" && pwd)"

TARGET_REF="refs/heads/${TARGET_BRANCH}"
if ! git -C "$PRIMARY_ROOT" rev-parse --verify --quiet "${TARGET_REF}^{commit}" >/dev/null; then
  echo "ERROR: target branch '${TARGET_BRANCH}' does not exist in ${PRIMARY_ROOT}" >&2
  exit 1
fi

PRIMARY_BRANCH="$(git -C "$PRIMARY_ROOT" symbolic-ref --quiet --short HEAD 2>/dev/null || true)"

format_mib() {
  local kib="$1"
  awk -v k="$kib" 'BEGIN { printf "%.1f MiB", k / 1024.0 }'
}

# Check whether any live OS process has its CWD inside the given canonical directory.
# Outputs a comma-separated list of PIDs if active, or empty string if idle.
# Exits 2 if OS process CWD inspection is unavailable (fail safe).
find_active_cwd_pids() {
  local wt_real="$1"
  local pids=()
  local self_pid="$$"
  local bash_pid="${BASHPID:-$$}"

  if [[ -d /proc ]]; then
    local proc_entry pid cwd_path
    for proc_entry in /proc/[0-9]*; do
      [[ -e "$proc_entry/cwd" ]] || continue
      pid="${proc_entry#/proc/}"
      if [[ "$pid" == "$self_pid" || "$pid" == "$bash_pid" ]]; then
        continue
      fi
      cwd_path="$(readlink -f "$proc_entry/cwd" 2>/dev/null || true)"
      if [[ -z "$cwd_path" ]]; then
        continue
      fi
      if [[ "$cwd_path" == "$wt_real" || "$cwd_path" == "$wt_real/"* ]]; then
        pids+=("$pid")
      fi
    done
    if [[ ${#pids[@]} -gt 0 ]]; then
      local IFS=","
      echo "${pids[*]}"
    fi
    return 0
  fi

  if command -v lsof >/dev/null 2>&1; then
    local cur_pid="" line
    while IFS= read -r line; do
      case "$line" in
        p*)
          cur_pid="${line#p}"
          ;;
        n*)
          local path="${line#n}"
          if [[ "$cur_pid" != "$self_pid" && "$cur_pid" != "$bash_pid" ]]; then
            if [[ "$path" == "$wt_real" || "$path" == "$wt_real/"* ]]; then
              pids+=("$cur_pid")
            fi
          fi
          ;;
      esac
    done < <(lsof -d cwd -Fpn 2>/dev/null || true)
    if [[ ${#pids[@]} -gt 0 ]]; then
      local IFS=","
      echo "${pids[*]}"
    fi
    return 0
  fi

  return 2
}

# Parse `git worktree list --porcelain` into parallel arrays.
WT_PATHS=()
WT_HEADS=()
WT_BRANCHES=()
WT_LOCKED=()
WT_PRUNABLE=()

cur_path=""
cur_head=""
cur_branch=""
cur_locked="0"
cur_prunable="0"

flush_wt_record() {
  if [[ -n "$cur_path" ]]; then
    WT_PATHS+=("$cur_path")
    WT_HEADS+=("$cur_head")
    WT_BRANCHES+=("$cur_branch")
    WT_LOCKED+=("$cur_locked")
    WT_PRUNABLE+=("$cur_prunable")
  fi
  cur_path=""
  cur_head=""
  cur_branch=""
  cur_locked="0"
  cur_prunable="0"
}

while IFS= read -r line || [[ -n "$line" ]]; do
  if [[ -z "$line" ]]; then
    flush_wt_record
    continue
  fi
  case "$line" in
    worktree\ *)
      cur_path="${line#worktree }"
      ;;
    HEAD\ *)
      cur_head="${line#HEAD }"
      ;;
    branch\ refs/heads/*)
      cur_branch="${line#branch refs/heads/}"
      ;;
    branch\ *)
      cur_branch="${line#branch }"
      ;;
    locked|locked\ *)
      cur_locked="1"
      ;;
    prunable|prunable\ *)
      cur_prunable="1"
      ;;
  esac
done < <(git -C "$PRIMARY_ROOT" worktree list --porcelain)
flush_wt_record

echo "=== Worktree & Branch Cleanup Audit (mode=${MODE}, target=${TARGET_BRANCH}, repo=${PRIMARY_ROOT}) ==="

eligible_wt=0
removed_wt=0
kept_wt=0
prunable_wt=0
failed_wt=0
reclaimable_kib=0
freed_kib=0
eligible_branches=0
deleted_branches=0

for i in "${!WT_PATHS[@]}"; do
  wt="${WT_PATHS[$i]}"
  wt_head="${WT_HEADS[$i]}"
  wt_branch="${WT_BRANCHES[$i]}"
  wt_locked="${WT_LOCKED[$i]}"
  wt_prunable="${WT_PRUNABLE[$i]}"

  if [[ -d "$wt" ]]; then
    wt_real="$(cd -P "$wt" && pwd)"
  else
    wt_real="$wt"
  fi

  # Always skip primary checkout.
  if [[ "$wt_real" == "$PRIMARY_ROOT" ]]; then
    continue
  fi

  short_head="${wt_head:0:10}"
  branch_label="${wt_branch:-<detached>}"

  # Handle stale metadata whose directory is already absent.
  if [[ ! -d "$wt" || "$wt_prunable" == "1" ]]; then
    prunable_wt=$((prunable_wt + 1))
    if [[ "$MODE" == "apply" ]]; then
      echo "WORKTREE PRUNABLE-STALE | branch=${branch_label} | head=${short_head} | size=0.0 MiB | path=${wt}"
    else
      echo "WORKTREE PRUNABLE-STALE | branch=${branch_label} | head=${short_head} | size=0.0 MiB | path=${wt}"
    fi
    continue
  fi

  size_kib="$(du -sk "$wt_real" 2>/dev/null | awk '{print $1}')"
  size_kib="${size_kib:-0}"
  size_human="$(format_mib "$size_kib")"

  reasons=()

  if [[ "$wt_locked" == "1" ]]; then
    reasons+=("LOCKED")
  fi

  if [[ "$CALLER_PWD" == "$wt_real" || "$CALLER_PWD" == "$wt_real/"* ]]; then
    reasons+=("ACTIVE:caller-cwd")
  fi

  # Safety check (a): working tree must be 100% clean.
  if ! status_out="$(git -C "$wt_real" status --porcelain --untracked-files=all 2>/dev/null)"; then
    reasons+=("DIRTY:status-error")
  elif [[ -n "$status_out" ]]; then
    dirty_count="$(printf '%s\n' "$status_out" | wc -l | tr -d ' ')"
    reasons+=("DIRTY:${dirty_count}")
  fi

  # Safety check (b): worktree HEAD must be an ancestor of target branch.
  if [[ -z "$wt_head" ]] || ! git -C "$PRIMARY_ROOT" merge-base --is-ancestor "$wt_head" "$TARGET_REF" 2>/dev/null; then
    reasons+=("UNMERGED")
  fi

  # Safety check (c): no live OS process has its CWD inside the worktree.
  active_pids=""
  if active_pids="$(find_active_cwd_pids "$wt_real")"; then
    if [[ -n "$active_pids" ]]; then
      reasons+=("ACTIVE:pids=${active_pids}")
    fi
  else
    reasons+=("ACTIVE-CHECK-UNAVAILABLE")
  fi

  if [[ ${#reasons[@]} -gt 0 ]]; then
    kept_wt=$((kept_wt + 1))
    reason_str="$(IFS='; '; echo "${reasons[*]}")"
    echo "WORKTREE KEPT (${reason_str}) | branch=${branch_label} | head=${short_head} | size=${size_human} | path=${wt_real}"
    continue
  fi

  eligible_wt=$((eligible_wt + 1))
  reclaimable_kib=$((reclaimable_kib + size_kib))

  if [[ "$MODE" == "dry-run" ]]; then
    echo "WORKTREE MERGED+SAFE-TO-CLEAN | branch=${branch_label} | head=${short_head} | size=${size_human} | path=${wt_real}"
  else
    if git -C "$PRIMARY_ROOT" worktree remove "$wt_real"; then
      removed_wt=$((removed_wt + 1))
      freed_kib=$((freed_kib + size_kib))
      branch_note="detached"
      if [[ -n "$wt_branch" && "$wt_branch" != "$TARGET_BRANCH" && "$wt_branch" != "$PRIMARY_BRANCH" ]]; then
        if git -C "$PRIMARY_ROOT" branch -d "$wt_branch" >/dev/null 2>&1; then
          deleted_branches=$((deleted_branches + 1))
          branch_note="branch-deleted:${wt_branch}"
        else
          branch_note="branch-retained:${wt_branch}"
        fi
      fi
      echo "WORKTREE REMOVED | branch=${branch_label} (${branch_note}) | head=${short_head} | freed=${size_human} | path=${wt_real}"
    else
      failed_wt=$((failed_wt + 1))
      kept_wt=$((kept_wt + 1))
      echo "WORKTREE KEPT (REMOVE-FAILED) | branch=${branch_label} | head=${short_head} | size=${size_human} | path=${wt_real}" >&2
    fi
  fi
done

if [[ "$MODE" == "apply" ]]; then
  git -C "$PRIMARY_ROOT" worktree prune
fi

# Gather branches currently checked out by remaining worktrees so we never try to delete them.
declare -A CHECKED_OUT_BRANCHES=()
if [[ -n "$PRIMARY_BRANCH" ]]; then
  CHECKED_OUT_BRANCHES["$PRIMARY_BRANCH"]=1
fi
CHECKED_OUT_BRANCHES["$TARGET_BRANCH"]=1

while IFS= read -r line; do
  if [[ "$line" == branch\ refs/heads/* ]]; then
    br="${line#branch refs/heads/}"
    CHECKED_OUT_BRANCHES["$br"]=1
  fi
done < <(git -C "$PRIMARY_ROOT" worktree list --porcelain)

# Inspect any remaining local branches already merged into target branch.
while IFS= read -r br; do
  [[ -n "$br" ]] || continue
  if [[ -n "${CHECKED_OUT_BRANCHES[$br]:-}" ]]; then
    continue
  fi
  if git -C "$PRIMARY_ROOT" merge-base --is-ancestor "refs/heads/${br}" "$TARGET_REF" 2>/dev/null; then
    eligible_branches=$((eligible_branches + 1))
    if [[ "$MODE" == "dry-run" ]]; then
      echo "BRANCH MERGED+SAFE-TO-DELETE | branch=${br}"
    else
      if git -C "$PRIMARY_ROOT" branch -d "$br" >/dev/null 2>&1; then
        deleted_branches=$((deleted_branches + 1))
        echo "BRANCH DELETED | branch=${br}"
      else
        echo "BRANCH KEPT (DELETE-FAILED) | branch=${br}" >&2
      fi
    fi
  fi
done < <(git -C "$PRIMARY_ROOT" for-each-ref --format='%(refname:short)' refs/heads)

echo "=== Summary ==="
if [[ "$MODE" == "dry-run" ]]; then
  echo "Mode: dry-run (no changes made; re-run with --apply to reclaim)"
  echo "Secondary worktrees: eligible=${eligible_wt}, kept=${kept_wt}, prunable=${prunable_wt}"
  echo "Extra merged local branches: eligible=${eligible_branches}"
  echo "Estimated disk reclaimable: $(format_mib "$reclaimable_kib") (${reclaimable_kib} KiB)"
else
  echo "Mode: apply"
  echo "Secondary worktrees: removed=${removed_wt}, kept=${kept_wt}, prunable_pruned=${prunable_wt}, failed=${failed_wt}"
  echo "Merged local branches deleted: ${deleted_branches}"
  echo "Disk freed from removed worktrees: $(format_mib "$freed_kib") (${freed_kib} KiB)"
fi

echo "=== Disk Usage (df -h /) ==="
df -h /

if [[ "$failed_wt" -gt 0 ]]; then
  exit 1
fi
