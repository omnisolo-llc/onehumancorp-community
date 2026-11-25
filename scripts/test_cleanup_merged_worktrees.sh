#!/usr/bin/env bash
set -euo pipefail

# End-to-end real-git verification suite for scripts/cleanup_merged_worktrees.sh.
# Exercises real git worktrees, branches, dirty states, locks, and live OS process CWDs.

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
CLEANUP_SCRIPT="${SCRIPT_DIR}/cleanup_merged_worktrees.sh"

TMP_ROOT="$(mktemp -d /tmp/ohc-test-cleanup-wt.XXXXXX)"
ACTIVE_PID=""
PASS_COUNT=0

cleanup_tmp() {
  if [[ -n "$ACTIVE_PID" ]] && kill -0 "$ACTIVE_PID" 2>/dev/null; then
    kill "$ACTIVE_PID" 2>/dev/null || true
    wait "$ACTIVE_PID" 2>/dev/null || true
  fi
  rm -rf "$TMP_ROOT"
}
trap cleanup_tmp EXIT

assert_ok() {
  local desc="$1"
  shift
  if "$@"; then
    PASS_COUNT=$((PASS_COUNT + 1))
    echo "[ok] ${desc}"
  else
    echo "[FAIL] ${desc}" >&2
    exit 1
  fi
}

assert_contains() {
  local haystack="$1"
  local needle="$2"
  local desc="$3"
  if printf '%s\n' "$haystack" | grep -Fq -- "$needle"; then
    PASS_COUNT=$((PASS_COUNT + 1))
    echo "[ok] ${desc}"
  else
    echo "[FAIL] ${desc} (missing: ${needle})" >&2
    echo "--- actual output ---" >&2
    printf '%s\n' "$haystack" >&2
    exit 1
  fi
}

assert_not_exists() {
  local path="$1"
  local desc="$2"
  if [[ ! -e "$path" ]]; then
    PASS_COUNT=$((PASS_COUNT + 1))
    echo "[ok] ${desc}"
  else
    echo "[FAIL] ${desc} (still exists: ${path})" >&2
    exit 1
  fi
}

assert_exists() {
  local path="$1"
  local desc="$2"
  if [[ -e "$path" ]]; then
    PASS_COUNT=$((PASS_COUNT + 1))
    echo "[ok] ${desc}"
  else
    echo "[FAIL] ${desc} (missing: ${path})" >&2
    exit 1
  fi
}

assert_branch_exists() {
  local repo="$1"
  local br="$2"
  local desc="$3"
  if git -C "$repo" show-ref --verify --quiet "refs/heads/${br}"; then
    PASS_COUNT=$((PASS_COUNT + 1))
    echo "[ok] ${desc}"
  else
    echo "[FAIL] ${desc} (branch missing: ${br})" >&2
    exit 1
  fi
}

assert_branch_absent() {
  local repo="$1"
  local br="$2"
  local desc="$3"
  if ! git -C "$repo" show-ref --verify --quiet "refs/heads/${br}"; then
    PASS_COUNT=$((PASS_COUNT + 1))
    echo "[ok] ${desc}"
  else
    echo "[FAIL] ${desc} (branch still exists: ${br})" >&2
    exit 1
  fi
}

echo "=== Running cleanup_merged_worktrees.sh end-to-end tests ==="

assert_ok "cleanup script is executable" test -x "$CLEANUP_SCRIPT"

help_out="$("$CLEANUP_SCRIPT" --help)"
assert_contains "$help_out" "--dry-run" "--help documents --dry-run"
assert_contains "$help_out" "--apply" "--help documents --apply"

if "$CLEANUP_SCRIPT" --unknown-flag >/dev/null 2>&1; then
  echo "[FAIL] unknown flag should fail closed" >&2
  exit 1
else
  PASS_COUNT=$((PASS_COUNT + 1))
  echo "[ok] unknown flag fails closed"
fi

REPO="${TMP_ROOT}/repo"
WT_BASE="${TMP_ROOT}/worktrees"
mkdir -p "$REPO" "$WT_BASE"

git -C "$REPO" init -b main >/dev/null
git -C "$REPO" config user.name "OneHumanCorp Maintainers"
git -C "$REPO" config user.email "maintainers@onehumancorp.local"

printf 'base\n' > "${REPO}/README.md"
git -C "$REPO" add README.md
git -C "$REPO" commit -m "init: base commit" >/dev/null

# 1. Clean merged worktree (MUST be removed)
WT_MERGED_CLEAN="${WT_BASE}/wt-merged-clean"
git -C "$REPO" worktree add -b feat/merged-clean "$WT_MERGED_CLEAN" main >/dev/null 2>&1
printf 'merged feature\n' > "${WT_MERGED_CLEAN}/merged.txt"
git -C "$WT_MERGED_CLEAN" add merged.txt
git -C "$WT_MERGED_CLEAN" commit -m "feat: merged clean change" >/dev/null
git -C "$REPO" merge --no-ff -m "merge: feat/merged-clean" feat/merged-clean >/dev/null

# 2. Clean UNMERGED worktree (MUST be kept)
WT_UNMERGED="${WT_BASE}/wt-unmerged"
git -C "$REPO" worktree add -b feat/unmerged "$WT_UNMERGED" main >/dev/null 2>&1
printf 'pending work\n' > "${WT_UNMERGED}/pending.txt"
git -C "$WT_UNMERGED" add pending.txt
git -C "$WT_UNMERGED" commit -m "feat: unmerged work" >/dev/null

# 3. Merged worktree with dirty tracked modification (MUST be kept)
WT_DIRTY_TRACKED="${WT_BASE}/wt-dirty-tracked"
git -C "$REPO" worktree add -b feat/dirty-tracked "$WT_DIRTY_TRACKED" main >/dev/null 2>&1
printf 'local modification\n' >> "${WT_DIRTY_TRACKED}/README.md"

# 4. Merged worktree with untracked file (MUST be kept)
WT_DIRTY_UNTRACKED="${WT_BASE}/wt-dirty-untracked"
git -C "$REPO" worktree add -b feat/dirty-untracked "$WT_DIRTY_UNTRACKED" main >/dev/null 2>&1
printf 'untracked note\n' > "${WT_DIRTY_UNTRACKED}/scratch.txt"

# 5. Clean merged worktree with a live OS process whose CWD is inside it (MUST be kept)
WT_ACTIVE="${WT_BASE}/wt-active-cwd"
git -C "$REPO" worktree add -b feat/active-cwd "$WT_ACTIVE" main >/dev/null 2>&1
(cd "$WT_ACTIVE" && exec tail -f /dev/null) >/dev/null 2>&1 &
ACTIVE_PID=$!
# Wait briefly until /proc/$ACTIVE_PID/cwd resolves inside WT_ACTIVE
for _ in 1 2 3 4 5 6 7 8 9 10; do
  if [[ -d /proc ]] && [[ "$(readlink -f "/proc/${ACTIVE_PID}/cwd" 2>/dev/null || true)" == "$WT_ACTIVE" ]]; then
    break
  fi
  sleep 0.05
done

# 6. Clean merged LOCKED worktree (MUST be kept)
WT_LOCKED="${WT_BASE}/wt-locked"
git -C "$REPO" worktree add -b feat/locked "$WT_LOCKED" main >/dev/null 2>&1
git -C "$REPO" worktree lock --reason "reserved" "$WT_LOCKED"

# 7. Extra merged non-worktree local branch (MUST be deleted)
git -C "$REPO" checkout -b feat/orphan-merged main >/dev/null 2>&1
printf 'orphan merged\n' > "${REPO}/orphan.txt"
git -C "$REPO" add orphan.txt
git -C "$REPO" commit -m "feat: orphan merged branch" >/dev/null
git -C "$REPO" checkout main >/dev/null 2>&1
git -C "$REPO" merge --no-ff -m "merge: feat/orphan-merged" feat/orphan-merged >/dev/null

# 8. Extra unmerged non-worktree local branch (MUST be kept)
git -C "$REPO" checkout -b feat/orphan-unmerged main >/dev/null 2>&1
printf 'orphan unmerged\n' > "${REPO}/orphan_unmerged.txt"
git -C "$REPO" add orphan_unmerged.txt
git -C "$REPO" commit -m "feat: orphan unmerged branch" >/dev/null
git -C "$REPO" checkout main >/dev/null 2>&1

# Run default (--dry-run) and verify reporting + zero mutations.
dry_out="$("$CLEANUP_SCRIPT" --repo "$REPO")"
assert_contains "$dry_out" "WORKTREE MERGED+SAFE-TO-CLEAN | branch=feat/merged-clean" "dry-run marks clean merged worktree safe"
assert_contains "$dry_out" "WORKTREE KEPT (UNMERGED) | branch=feat/unmerged" "dry-run keeps unmerged worktree"
assert_contains "$dry_out" "WORKTREE KEPT (DIRTY:1) | branch=feat/dirty-tracked" "dry-run keeps dirty tracked worktree"
assert_contains "$dry_out" "WORKTREE KEPT (DIRTY:1) | branch=feat/dirty-untracked" "dry-run keeps dirty untracked worktree"
assert_contains "$dry_out" "ACTIVE:pids=" "dry-run keeps worktree with active OS process CWD"
assert_contains "$dry_out" "WORKTREE KEPT (LOCKED) | branch=feat/locked" "dry-run keeps locked worktree"
assert_contains "$dry_out" "BRANCH MERGED+SAFE-TO-DELETE | branch=feat/orphan-merged" "dry-run flags extra merged branch"
assert_exists "$WT_MERGED_CLEAN" "dry-run does not delete eligible worktree"
assert_branch_exists "$REPO" "feat/merged-clean" "dry-run does not delete eligible worktree branch"
assert_branch_exists "$REPO" "feat/orphan-merged" "dry-run does not delete extra merged branch"

# Run --apply and verify exact removals and preservations.
apply_out="$("$CLEANUP_SCRIPT" --repo "$REPO" --apply)"
assert_contains "$apply_out" "WORKTREE REMOVED | branch=feat/merged-clean (branch-deleted:feat/merged-clean)" "apply removes clean merged worktree and branch"
assert_contains "$apply_out" "BRANCH DELETED | branch=feat/orphan-merged" "apply deletes extra merged branch"
assert_contains "$apply_out" "Disk freed from removed worktrees:" "apply reports freed MiB"
assert_contains "$apply_out" "Disk Usage (df -h /)" "apply reports df -h /"

assert_not_exists "$WT_MERGED_CLEAN" "clean merged worktree directory removed"
assert_branch_absent "$REPO" "feat/merged-clean" "clean merged worktree branch deleted"
assert_branch_absent "$REPO" "feat/orphan-merged" "extra merged branch deleted"

assert_exists "$WT_UNMERGED" "unmerged worktree preserved"
assert_branch_exists "$REPO" "feat/unmerged" "unmerged branch preserved"
assert_exists "$WT_DIRTY_TRACKED" "dirty tracked worktree preserved"
assert_branch_exists "$REPO" "feat/dirty-tracked" "dirty tracked branch preserved"
assert_exists "$WT_DIRTY_UNTRACKED" "dirty untracked worktree preserved"
assert_branch_exists "$REPO" "feat/dirty-untracked" "dirty untracked branch preserved"
assert_exists "$WT_ACTIVE" "active-CWD worktree preserved"
assert_branch_exists "$REPO" "feat/active-cwd" "active-CWD branch preserved"
assert_exists "$WT_LOCKED" "locked worktree preserved"
assert_branch_exists "$REPO" "feat/locked" "locked branch preserved"
assert_branch_exists "$REPO" "feat/orphan-unmerged" "unmerged orphan branch preserved"
assert_branch_exists "$REPO" "main" "target branch main preserved"

# Stop the live OS process in WT_ACTIVE and verify --apply now reclaims it cleanly.
kill "$ACTIVE_PID" 2>/dev/null || true
wait "$ACTIVE_PID" 2>/dev/null || true
ACTIVE_PID=""

apply2_out="$("$CLEANUP_SCRIPT" --repo "$REPO" --apply)"
assert_contains "$apply2_out" "WORKTREE REMOVED | branch=feat/active-cwd (branch-deleted:feat/active-cwd)" "apply removes previously-active worktree once process exits"
assert_not_exists "$WT_ACTIVE" "idle merged worktree directory removed on second pass"
assert_branch_absent "$REPO" "feat/active-cwd" "idle merged worktree branch deleted on second pass"

echo "=== ALL ${PASS_COUNT} cleanup_merged_worktrees assertions PASSED ==="
