#!/usr/bin/env bash
# Real end-to-end tests for scripts/env-vault.sh and ergonomic wrappers.
# Uses a real temporary git repository, real OpenSSL encryption + HMAC-SHA256
# authentication, real file permissions, and real .gitignore checks.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
VAULT_BIN="${SCRIPT_DIR}/env-vault.sh"
STORE_BIN="${SCRIPT_DIR}/env-store.sh"
LOCK_BIN="${SCRIPT_DIR}/env-lock.sh"
UNLOCK_BIN="${SCRIPT_DIR}/env-unlock.sh"

TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/ohc-test-env-vault.XXXXXX")"
cleanup() {
  rm -rf "${TMP_ROOT}"
}
trap cleanup EXIT INT TERM

PASS_COUNT=0
assert_eq() {
  local expected="$1"
  local actual="$2"
  local msg="$3"
  if [[ "${expected}" != "${actual}" ]]; then
    printf 'FAIL: %s (expected=%q, actual=%q)\n' "${msg}" "${expected}" "${actual}" >&2
    exit 1
  fi
  PASS_COUNT=$((PASS_COUNT + 1))
}

assert_file_mode() {
  local path="$1"
  local expected="$2"
  local actual
  actual="$(python3 -c 'import os, stat, sys; print(f"{stat.S_IMODE(os.lstat(sys.argv[1]).st_mode):04o}")' "${path}")"
  assert_eq "${expected}" "${actual}" "file mode of $(basename "${path}")"
}

assert_not_contains() {
  local haystack="$1"
  local needle="$2"
  local msg="$3"
  if [[ "${haystack}" == *"${needle}"* ]]; then
    printf 'FAIL: %s (unexpectedly found secret substring)\n' "${msg}" >&2
    exit 1
  fi
  PASS_COUNT=$((PASS_COUNT + 1))
}

printf '=== Running env-vault end-to-end tests ===\n'

# 0. Verify repository .gitignore rules on the real workspace
git -C "${REPO_ROOT}" check-ignore -q .env
git -C "${REPO_ROOT}" check-ignore -q .env.local
git -C "${REPO_ROOT}" check-ignore -q .env.production.local
git -C "${REPO_ROOT}" check-ignore -q .env.decrypted
git -C "${REPO_ROOT}" check-ignore -q sample.pem
git -C "${REPO_ROOT}" check-ignore -q sample.key
git -C "${REPO_ROOT}" check-ignore -q .ohc-env.key
if git -C "${REPO_ROOT}" check-ignore -q .env.example; then
  printf 'FAIL: .env.example should NOT be gitignored\n' >&2
  exit 1
fi
if git -C "${REPO_ROOT}" check-ignore -q .env.enc; then
  printf 'FAIL: .env.enc should NOT be gitignored\n' >&2
  exit 1
fi
PASS_COUNT=$((PASS_COUNT + 9))
printf '[ok] workspace .gitignore protections verified\n'

# 1. Initialize real temporary git repo with the repo's .gitignore
WORK_REPO="${TMP_ROOT}/repo"
mkdir -p "${WORK_REPO}"
git -C "${WORK_REPO}" init -q
cp "${REPO_ROOT}/.gitignore" "${WORK_REPO}/.gitignore"
git -C "${WORK_REPO}" add .gitignore
git -C "${WORK_REPO}" -c user.name="OneHumanCorp Maintainers" -c user.email="maintainers@onehumancorp.local" commit -q -m "init"

# 2. Store secrets (simple, special characters, stdin, and key update) without leaking values
SECRET_SIMPLE="sk-live-alpha-987654321"
SECRET_COMPLEX='p@ss word "quoted" $dollar `tick` \slash #hash'
SECRET_STDIN="stdin-secret-token-42"

store_out_1="$("${STORE_BIN}" --repo-root "${WORK_REPO}" OPENAI_API_KEY "${SECRET_SIMPLE}")"
assert_not_contains "${store_out_1}" "${SECRET_SIMPLE}" "store stdout must not reveal simple secret"

store_out_2="$("${VAULT_BIN}" --repo-root "${WORK_REPO}" set COMPLEX_SECRET "${SECRET_COMPLEX}")"
assert_not_contains "${store_out_2}" "${SECRET_COMPLEX}" "set stdout must not reveal complex secret"

store_out_3="$(printf '%s\n' "${SECRET_STDIN}" | "${STORE_BIN}" --repo-root "${WORK_REPO}" STRIPE_SECRET_KEY --stdin)"
assert_not_contains "${store_out_3}" "${SECRET_STDIN}" "store --stdin stdout must not reveal secret"

# Update an existing key in place (no duplicate keys)
SECRET_UPDATED="sk-live-rotated-11223344"
store_out_4="$("${STORE_BIN}" --repo-root "${WORK_REPO}" OPENAI_API_KEY "${SECRET_UPDATED}")"
assert_not_contains "${store_out_4}" "${SECRET_UPDATED}" "updated secret must not appear in stdout"

assert_file_mode "${WORK_REPO}/.env" "0600"
key_list="$("${VAULT_BIN}" --repo-root "${WORK_REPO}" keys)"
assert_eq "$(printf 'OPENAI_API_KEY\nCOMPLEX_SECRET\nSTRIPE_SECRET_KEY')" "${key_list}" "keys list exact match without duplicates"
assert_not_contains "${key_list}" "${SECRET_UPDATED}" "keys output must never contain values"

# Save exact plaintext bytes for byte-for-byte comparison after lock -> unlock
EXPECTED_SNAPSHOT="${TMP_ROOT}/expected.env"
cp "${WORK_REPO}/.env" "${EXPECTED_SNAPSHOT}"
chmod 600 "${EXPECTED_SNAPSHOT}"
printf '[ok] store/set/update/keys verified with 0600 mode and zero secret echo\n'

# 3. Verify status in unlocked state
status_unlocked="$("${VAULT_BIN}" --repo-root "${WORK_REPO}" status)"
assert_eq "1" "$(printf '%s\n' "${status_unlocked}" | grep -c '^state=unlocked$' || true)" "status reports unlocked"
assert_eq "1" "$(printf '%s\n' "${status_unlocked}" | grep -c '^git_ignored=ok$' || true)" "status reports git_ignored=ok"
assert_eq "1" "$(printf '%s\n' "${status_unlocked}" | grep -c '^env_permissions=ok (0600)$' || true)" "status reports 0600 mode"
printf '[ok] status in unlocked state verified\n'

# 4. Encrypt with --keep-plaintext (encrypt command default)
TEST_PASS="correct-horse-battery-staple-2026!"
OHC_ENV_PASSPHRASE="${TEST_PASS}" "${VAULT_BIN}" --repo-root "${WORK_REPO}" encrypt >/dev/null
[[ -f "${WORK_REPO}/.env" ]] || { printf 'FAIL: encrypt should keep plaintext by default\n' >&2; exit 1; }
[[ -f "${WORK_REPO}/.env.enc" ]] || { printf 'FAIL: encrypt did not create .env.enc\n' >&2; exit 1; }
PASS_COUNT=$((PASS_COUNT + 2))

# 5. Lock via wrapper -> removes plaintext after round-trip verification
OHC_ENV_PASSPHRASE="${TEST_PASS}" "${LOCK_BIN}" --repo-root "${WORK_REPO}" >/dev/null
if [[ -e "${WORK_REPO}/.env" ]]; then
  printf 'FAIL: lock must remove plaintext .env after round-trip verification\n' >&2
  exit 1
fi
[[ -f "${WORK_REPO}/.env.enc" ]] || { printf 'FAIL: .env.enc missing after lock\n' >&2; exit 1; }
# Ensure plaintext secrets do not appear anywhere in .env.enc
if grep -Fq "${SECRET_UPDATED}" "${WORK_REPO}/.env.enc" || grep -Fq "${SECRET_STDIN}" "${WORK_REPO}/.env.enc"; then
  printf 'FAIL: plaintext secret leaked into .env.enc\n' >&2
  exit 1
fi
PASS_COUNT=$((PASS_COUNT + 3))

# Confirm git status sees .env.enc as trackable and never sees .env
untracked_files="$(git -C "${WORK_REPO}" status --porcelain)"
assert_eq "?? .env.enc" "${untracked_files}" "git sees .env.enc as trackable and ignores .env"

# Verify status + cryptographic verify in locked state
status_locked="$("${VAULT_BIN}" --repo-root "${WORK_REPO}" status)"
assert_eq "1" "$(printf '%s\n' "${status_locked}" | grep -c '^state=locked$' || true)" "status reports locked"
verify_mac_out="$(OHC_ENV_PASSPHRASE="${TEST_PASS}" "${VAULT_BIN}" --repo-root "${WORK_REPO}" verify --check-mac --require-vault)"
assert_eq "1" "$(printf '%s\n' "${verify_mac_out}" | grep -c '^vault_mac=ok$' || true)" "verify --check-mac succeeds"
printf '[ok] lock, plaintext removal, git visibility, and verify --check-mac verified\n'

# 6. Wrong passphrase MUST fail closed without creating .env
if OHC_ENV_PASSPHRASE="wrong-passphrase-999" "${UNLOCK_BIN}" --repo-root "${WORK_REPO}" >/dev/null 2>&1; then
  printf 'FAIL: unlock must fail on wrong passphrase\n' >&2
  exit 1
fi
if [[ -e "${WORK_REPO}/.env" ]]; then
  printf 'FAIL: failed unlock created .env\n' >&2
  exit 1
fi
PASS_COUNT=$((PASS_COUNT + 2))
printf '[ok] wrong passphrase fails closed\n'

# 7. Tampered ciphertext MUST fail closed (flip 1 character inside ciphertext payload)
TAMPERED_VAULT="${TMP_ROOT}/tampered.env.enc"
python3 - "${WORK_REPO}/.env.enc" "${TAMPERED_VAULT}" <<'PY'
import sys
with open(sys.argv[1], "r", encoding="ascii") as f:
    lines = f.read().splitlines()
for i, line in enumerate(lines):
    if line == "-----BEGIN OHC ENCRYPTED ENV-----":
        target = lines[i + 1]
        flipped = ("A" if target[0] != "A" else "B") + target[1:]
        lines[i + 1] = flipped
        break
with open(sys.argv[2], "w", encoding="ascii") as out:
    out.write("\n".join(lines) + "\n")
PY
if OHC_ENV_PASSPHRASE="${TEST_PASS}" "${VAULT_BIN}" --repo-root "${WORK_REPO}" --vault-file "${TAMPERED_VAULT}" unlock >/dev/null 2>&1; then
  printf 'FAIL: unlock must fail closed on tampered ciphertext\n' >&2
  exit 1
fi
if [[ -e "${WORK_REPO}/.env" ]]; then
  printf 'FAIL: tampered unlock created .env\n' >&2
  exit 1
fi
PASS_COUNT=$((PASS_COUNT + 2))
printf '[ok] tampered ciphertext fails closed\n'

# 8. Unlock with correct passphrase -> exact byte-for-byte equality and 0600 mode
OHC_ENV_PASSPHRASE="${TEST_PASS}" "${UNLOCK_BIN}" --repo-root "${WORK_REPO}" >/dev/null
[[ -f "${WORK_REPO}/.env" ]] || { printf 'FAIL: unlock did not restore .env\n' >&2; exit 1; }
assert_file_mode "${WORK_REPO}/.env" "0600"
if ! cmp -s "${EXPECTED_SNAPSHOT}" "${WORK_REPO}/.env"; then
  printf 'FAIL: unlocked .env does not match original plaintext byte-for-byte\n' >&2
  exit 1
fi
PASS_COUNT=$((PASS_COUNT + 2))
printf '[ok] unlock restored byte-for-byte identical .env with mode 0600\n'

# 9. Failed unlock when .env already exists MUST NOT clobber existing .env
if OHC_ENV_PASSPHRASE="wrong-passphrase-again" "${UNLOCK_BIN}" --repo-root "${WORK_REPO}" >/dev/null 2>&1; then
  printf 'FAIL: wrong passphrase should fail when .env exists\n' >&2
  exit 1
fi
if ! cmp -s "${EXPECTED_SNAPSHOT}" "${WORK_REPO}/.env"; then
  printf 'FAIL: failed unlock clobbered existing .env\n' >&2
  exit 1
fi
PASS_COUNT=$((PASS_COUNT + 2))
printf '[ok] existing .env preserved untouched on failed unlock\n'

# 10. Keyfile mode (OHC_ENV_KEY_FILE) end-to-end lock -> unlock + loose permission & un-gitignored rejection
UNIGNORED_KEYFILE="${WORK_REPO}/unignored-passphrase.secret"
printf 'unignored-keyfile-entropy-99112\n' > "${UNIGNORED_KEYFILE}"
chmod 600 "${UNIGNORED_KEYFILE}"
if git -C "${WORK_REPO}" check-ignore -q unignored-passphrase.secret; then
  printf 'FAIL: test setup expected unignored-passphrase.secret to not be gitignored\n' >&2
  exit 1
fi
if OHC_ENV_KEY_FILE="${UNIGNORED_KEYFILE}" "${LOCK_BIN}" --repo-root "${WORK_REPO}" >/dev/null 2>&1; then
  printf 'FAIL: un-gitignored 0600 keyfile inside git repo must be rejected\n' >&2
  exit 1
fi
rm -f "${UNIGNORED_KEYFILE}"

KEYFILE="${WORK_REPO}/.ohc-env.key"
printf 'keyfile-passphrase-entropy-88421\n' > "${KEYFILE}"
chmod 644 "${KEYFILE}"
if OHC_ENV_KEY_FILE="${KEYFILE}" "${LOCK_BIN}" --repo-root "${WORK_REPO}" >/dev/null 2>&1; then
  printf 'FAIL: insecure 0644 keyfile should be rejected\n' >&2
  exit 1
fi
chmod 600 "${KEYFILE}"
OHC_ENV_KEY_FILE="${KEYFILE}" "${LOCK_BIN}" --repo-root "${WORK_REPO}" >/dev/null
[[ ! -e "${WORK_REPO}/.env" ]] || { printf 'FAIL: keyfile lock did not remove plaintext\n' >&2; exit 1; }
OHC_ENV_KEY_FILE="${KEYFILE}" "${UNLOCK_BIN}" --repo-root "${WORK_REPO}" >/dev/null
if ! cmp -s "${EXPECTED_SNAPSHOT}" "${WORK_REPO}/.env"; then
  printf 'FAIL: keyfile unlock mismatch\n' >&2
  exit 1
fi
PASS_COUNT=$((PASS_COUNT + 4))
printf '[ok] keyfile mode (0600 + gitignore enforced + lock/unlock round-trip) verified\n'

# 11. Fail closed when .env is NOT gitignored in a git repo
UNSAFE_REPO="${TMP_ROOT}/unsafe-repo"
mkdir -p "${UNSAFE_REPO}"
git -C "${UNSAFE_REPO}" init -q
if "${STORE_BIN}" --repo-root "${UNSAFE_REPO}" API_KEY "secret" >/dev/null 2>&1; then
  printf 'FAIL: store must refuse to write .env when .env is not gitignored\n' >&2
  exit 1
fi
PASS_COUNT=$((PASS_COUNT + 1))
printf '[ok] un-gitignored .env rejected cleanly\n'

printf '=== ALL %d env-vault assertions PASSED ===\n' "${PASS_COUNT}"
