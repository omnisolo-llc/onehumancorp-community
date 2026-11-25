#!/usr/bin/env bash
# OneHumanCorp .env Secret Vault
# Clean-room secret storage, authenticated encryption (lock/encrypt),
# authenticated decryption (unlock/decrypt), and hygiene verification.

set -euo pipefail
umask 077

VAULT_MAGIC="OHC_ENV_VAULT_V1"
BACKEND_OPENSSL="openssl-aes256cbc-etm"
BACKEND_AGE="age-armored"
KDF_NAME="pbkdf2-sha256"
KDF_ITERATIONS="600000"

REPO_ROOT="${OHC_REPO_ROOT:-}"
ENV_FILE="${OHC_ENV_FILE:-}"
VAULT_FILE="${OHC_ENV_VAULT_FILE:-}"
KEY_FILE="${OHC_ENV_KEY_FILE:-}"
BACKEND="${OHC_ENV_BACKEND:-openssl}"

TEMP_DIR=""
STAGING_TEMP_FILE=""
cleanup() {
  if [[ -n "${STAGING_TEMP_FILE}" && -e "${STAGING_TEMP_FILE}" ]]; then
    rm -f "${STAGING_TEMP_FILE}"
  fi
  if [[ -n "${TEMP_DIR}" && -d "${TEMP_DIR}" ]]; then
    rm -rf "${TEMP_DIR}"
  fi
}
trap cleanup EXIT INT TERM

ensure_temp_dir() {
  if [[ -z "${TEMP_DIR}" ]]; then
    TEMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/ohc-env-vault.XXXXXX")"
    chmod 700 "${TEMP_DIR}"
  fi
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: scripts/env-vault.sh <command> [options]

Commands:
  store | set <KEY> [VALUE] [--stdin]
      Store or update KEY=VALUE in gitignored .env (mode 0600).
      Secret values are never printed to stdout or logs.

  lock [--keep-plaintext] [--backend openssl|age]
      Encrypt .env -> .env.enc with authenticated integrity, verify a
      full round-trip decryption, and remove plaintext .env unless
      --keep-plaintext is provided.

  encrypt [--remove-plaintext] [--backend openssl|age]
      Encrypt .env -> .env.enc with authenticated integrity and verify
      a full round-trip decryption while keeping plaintext .env unless
      --remove-plaintext is provided.

  unlock | decrypt
      Verify authenticated integrity first, then atomically decrypt
      .env.enc -> .env (mode 0600). Fails closed on wrong passphrase or
      tampered ciphertext without modifying any existing .env.

  status
      Check gitignore/untracked status, file permissions, and .env.enc
      envelope structure without revealing secrets.

  verify [--check-mac] [--require-vault]
      Validate gitignore/untracked protection, permissions, and .env.enc
      structure. If --check-mac is set or a passphrase/keyfile is
      supplied, also verifies the cryptographic MAC tag.

  keys
      List variable names currently defined in .env (values are never printed).

Global options (may appear before or after the command):
  --repo-root <dir>     Repository root directory (default: git top-level or PWD)
  --env-file <path>     Plaintext env file path (default: <repo-root>/.env)
  --vault-file <path>   Encrypted vault path (default: <repo-root>/.env.enc)
  --key-file <path>     Path to gitignored 0600 passphrase/key file (env: OHC_ENV_KEY_FILE)
  --backend <name>      Encryption backend: openssl (default) or age
  -h, --help            Show this help text

Environment variables:
  OHC_ENV_PASSPHRASE    Passphrase for non-interactive lock/unlock/verify
  OHC_ENV_KEY_FILE      Path to gitignored 0600 passphrase/key file
  OHC_ENV_AGE_RECIPIENT Optional age public recipient (for --backend age)
EOF
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "Required command '$1' is not installed."
}

assert_not_symlink() {
  local target="$1"
  local label="$2"
  if [[ -L "${target}" ]]; then
    die "Refusing to operate on symlink for ${label}: ${target}"
  fi
}

get_file_mode() {
  local path="$1"
  python3 -c 'import os, stat, sys; print(f"{stat.S_IMODE(os.lstat(sys.argv[1]).st_mode):04o}")' "$path"
}

resolve_abs_path() {
  python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "$1"
}

resolve_paths() {
  if [[ -z "${REPO_ROOT}" ]]; then
    if git rev-parse --show-toplevel >/dev/null 2>&1; then
      REPO_ROOT="$(git rev-parse --show-toplevel)"
    else
      REPO_ROOT="$(pwd)"
    fi
  fi
  REPO_ROOT="$(resolve_abs_path "${REPO_ROOT}")"

  if [[ -z "${ENV_FILE}" ]]; then
    ENV_FILE="${REPO_ROOT}/.env"
  else
    ENV_FILE="$(resolve_abs_path "${ENV_FILE}")"
  fi

  if [[ -z "${VAULT_FILE}" ]]; then
    VAULT_FILE="${REPO_ROOT}/.env.enc"
  else
    VAULT_FILE="$(resolve_abs_path "${VAULT_FILE}")"
  fi

  if [[ -n "${KEY_FILE}" ]]; then
    KEY_FILE="$(resolve_abs_path "${KEY_FILE}")"
  fi
}

# Echoes "<git_root>\t<rel_path>" if target path lies inside a git work tree; returns 1 otherwise.
git_rel_path_for() {
  local target_abs="$1"
  local probe_dir
  probe_dir="$(dirname "${target_abs}")"
  while [[ ! -d "${probe_dir}" && "${probe_dir}" != "/" ]]; do
    probe_dir="$(dirname "${probe_dir}")"
  done
  if ! git -C "${probe_dir}" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    return 1
  fi
  local git_root
  git_root="$(git -C "${probe_dir}" rev-parse --show-toplevel)"
  python3 -c '
import os, sys
root = os.path.realpath(sys.argv[1])
target_dir = os.path.realpath(os.path.dirname(sys.argv[2]))
target = os.path.join(target_dir, os.path.basename(sys.argv[2]))
try:
    rel = os.path.relpath(target, root)
except ValueError:
    sys.exit(1)
if rel == ".." or rel.startswith("../"):
    sys.exit(1)
print(f"{root}\t{rel}")
' "${git_root}" "${target_abs}"
}

check_git_protection() {
  local target_abs="$1"
  local label="$2"
  local info
  if ! info="$(git_rel_path_for "${target_abs}")"; then
    return 0
  fi
  local git_root rel_path
  IFS=$'\t' read -r git_root rel_path <<<"${info}"

  if git -C "${git_root}" ls-files --error-unmatch -- "${rel_path}" >/dev/null 2>&1; then
    die "${label} (${rel_path}) is tracked in git! Run 'git rm --cached ${rel_path}' and verify .gitignore before proceeding."
  fi
  if ! git -C "${git_root}" check-ignore -q -- "${rel_path}"; then
    die "${label} (${rel_path}) is NOT gitignored! Add '${rel_path}' to .gitignore before storing plaintext secrets."
  fi
}

validate_key_file() {
  local label="$1"
  assert_not_symlink "${KEY_FILE}" "${label}"
  [[ -f "${KEY_FILE}" ]] || die "${label} does not exist: ${KEY_FILE}"
  local mode
  mode="$(get_file_mode "${KEY_FILE}")"
  if [[ "${mode}" != "0600" ]]; then
    die "${label} ${KEY_FILE} has insecure permissions (${mode}); expected 0600 (run 'chmod 600 ${KEY_FILE}')."
  fi
  check_git_protection "${KEY_FILE}" "${label}"
}

# Loads passphrase into a secure 0600 file inside TEMP_DIR and echoes its path.
# Passphrase is never passed via command-line arguments.
acquire_passphrase_file() {
  local confirm="${1:-0}"
  ensure_temp_dir
  local pass_file="${TEMP_DIR}/passphrase"

  if [[ -n "${KEY_FILE}" ]]; then
    validate_key_file "key file"
    python3 -c '
import sys
with open(sys.argv[1], "rb") as f:
    raw = f.read()
text = raw.decode("utf-8").rstrip("\r\n")
if not text:
    sys.exit(2)
with open(sys.argv[2], "wb") as out:
    out.write(text.encode("utf-8"))
' "${KEY_FILE}" "${pass_file}" || die "Key file is empty or unreadable: ${KEY_FILE}"
    chmod 600 "${pass_file}"
    printf '%s\n' "${pass_file}"
    return 0
  fi

  if [[ -n "${OHC_ENV_PASSPHRASE:-}" ]]; then
    python3 -c '
import os, sys
val = os.environ.get("OHC_ENV_PASSPHRASE", "")
if not val:
    sys.exit(2)
with open(sys.argv[1], "wb") as out:
    out.write(val.encode("utf-8"))
' "${pass_file}" || die "OHC_ENV_PASSPHRASE must be non-empty."
    chmod 600 "${pass_file}"
    printf '%s\n' "${pass_file}"
    return 0
  fi

  if [[ -r /dev/tty && -w /dev/tty ]]; then
    local p1="" p2=""
    printf 'Enter vault passphrase: ' >/dev/tty
    IFS= read -r -s p1 </dev/tty
    printf '\n' >/dev/tty
    [[ -n "${p1}" ]] || die "Passphrase cannot be empty."
    if [[ "${confirm}" == "1" ]]; then
      printf 'Confirm vault passphrase: ' >/dev/tty
      IFS= read -r -s p2 </dev/tty
      printf '\n' >/dev/tty
      [[ "${p1}" == "${p2}" ]] || die "Passphrases do not match."
    fi
    OHC_VAULT_PROMPT_PASS="${p1}" python3 -c '
import os, sys
with open(sys.argv[1], "wb") as out:
    out.write(os.environ["OHC_VAULT_PROMPT_PASS"].encode("utf-8"))
' "${pass_file}"
    unset p1 p2
    chmod 600 "${pass_file}"
    printf '%s\n' "${pass_file}"
    return 0
  fi

  die "No passphrase supplied. Set OHC_ENV_PASSPHRASE, pass --key-file / OHC_ENV_KEY_FILE, or run in an interactive TTY."
}

openssl_encrypt_to_file() {
  local src_plain="$1"
  local dest_vault="$2"
  local pass_file="$3"

  require_cmd openssl
  require_cmd python3
  ensure_temp_dir

  local ct_raw="${TEMP_DIR}/cipher.bin"
  openssl enc -aes-256-cbc -pbkdf2 -iter "${KDF_ITERATIONS}" -salt -md sha256 \
    -pass fd:3 -in "${src_plain}" -out "${ct_raw}" 3<"${pass_file}"

  local dest_dir
  dest_dir="$(dirname "${dest_vault}")"
  mkdir -p "${dest_dir}"
  local tmp_vault
  tmp_vault="$(mktemp "${dest_dir}/.env.enc.tmp.XXXXXX")"

  python3 - "${ct_raw}" "${tmp_vault}" "${VAULT_MAGIC}" "${BACKEND_OPENSSL}" "${KDF_NAME}" "${KDF_ITERATIONS}" 3<"${pass_file}" <<'PY'
import base64
import hashlib
import hmac
import os
import secrets
import sys

ct_path, out_path, magic, backend, kdf, iterations_str = sys.argv[1:7]
iterations = int(iterations_str)
with os.fdopen(3, "rb") as pf:
    passphrase = pf.read()
if not passphrase:
    raise SystemExit("empty passphrase")

with open(ct_path, "rb") as cf:
    ct_bytes = cf.read()
if not ct_bytes:
    raise SystemExit("empty ciphertext")

ct_b64_flat = base64.b64encode(ct_bytes).decode("ascii")
mac_salt_hex = secrets.token_hex(32)
mac_key = hashlib.pbkdf2_hmac(
    "sha256",
    passphrase,
    b"ohc-env-vault-mac-v1:" + bytes.fromhex(mac_salt_hex),
    iterations,
    dklen=32,
)
canonical = (
    f"{magic}\n"
    f"backend={backend}\n"
    f"kdf={kdf}\n"
    f"iterations={iterations}\n"
    f"mac_salt={mac_salt_hex}\n"
    f"ciphertext_b64={ct_b64_flat}\n"
).encode("ascii")
mac_hex = hmac.new(mac_key, canonical, hashlib.sha256).hexdigest()

wrapped = "\n".join(ct_b64_flat[i:i + 64] for i in range(0, len(ct_b64_flat), 64))
envelope = (
    f"{magic}\n"
    f"backend={backend}\n"
    f"kdf={kdf}\n"
    f"iterations={iterations}\n"
    f"mac_salt={mac_salt_hex}\n"
    f"mac_hmac_sha256={mac_hex}\n"
    f"-----BEGIN OHC ENCRYPTED ENV-----\n"
    f"{wrapped}\n"
    f"-----END OHC ENCRYPTED ENV-----\n"
)
with open(out_path, "wb") as out:
    out.write(envelope.encode("ascii"))
PY

  chmod 644 "${tmp_vault}"
  mv -f "${tmp_vault}" "${dest_vault}"
}

validate_vault_envelope() {
  local vault_path="$1"
  local verify_mac="${2:-0}"
  local pass_file="${3:-}"
  local extract_ct="${4:-}"

  require_cmd python3
  local fd3_source="/dev/null"
  if [[ "${verify_mac}" == "1" ]]; then
    fd3_source="${pass_file}"
  fi

  python3 - "${vault_path}" "${VAULT_MAGIC}" "${BACKEND_OPENSSL}" "${BACKEND_AGE}" "${KDF_NAME}" "${verify_mac}" "${extract_ct}" 3<"${fd3_source}" <<'PY'
import base64
import binascii
import hashlib
import hmac
import os
import re
import sys

vault_path, expected_magic, backend_openssl, backend_age, expected_kdf, check_mac, extract_ct = sys.argv[1:8]
with open(vault_path, "rb") as f:
    raw = f.read()

try:
    text = raw.decode("ascii")
except UnicodeDecodeError:
    raise SystemExit("Vault file is not valid ASCII envelope text.")

lines = [line.rstrip("\r") for line in text.splitlines()]
if len(lines) < 5 or lines[0] != expected_magic:
    raise SystemExit(f"Invalid vault header (expected '{expected_magic}').")

meta = {}
idx = 1
begin_marker = None
while idx < len(lines):
    line = lines[idx]
    if line.startswith("-----BEGIN "):
        begin_marker = line
        idx += 1
        break
    if "=" not in line:
        raise SystemExit(f"Malformed metadata line {idx + 1} in vault file.")
    k, v = line.split("=", 1)
    if k in meta:
        raise SystemExit(f"Duplicate metadata key '{k}' in vault file.")
    meta[k] = v
    idx += 1

if begin_marker is None:
    raise SystemExit("Missing ciphertext BEGIN marker in vault file.")

backend = meta.get("backend", "")
if backend == backend_openssl:
    if begin_marker != "-----BEGIN OHC ENCRYPTED ENV-----":
        raise SystemExit("Unexpected BEGIN marker for openssl backend.")
    end_marker = "-----END OHC ENCRYPTED ENV-----"
    if meta.get("kdf") != expected_kdf:
        raise SystemExit(f"Unsupported KDF '{meta.get('kdf')}' (expected '{expected_kdf}').")
    iter_str = meta.get("iterations", "")
    if not iter_str.isdigit() or int(iter_str) < 600000:
        raise SystemExit(f"Invalid PBKDF2 iteration count '{iter_str}' (minimum 600000).")
    iterations = int(iter_str)
    mac_salt_hex = meta.get("mac_salt", "")
    mac_hex = meta.get("mac_hmac_sha256", "")
    if not re.fullmatch(r"[0-9a-f]{64}", mac_salt_hex):
        raise SystemExit("Invalid mac_salt format (expected 64 lowercase hex chars).")
    if not re.fullmatch(r"[0-9a-f]{64}", mac_hex):
        raise SystemExit("Invalid mac_hmac_sha256 format (expected 64 lowercase hex chars).")

    payload_lines = []
    found_end = False
    while idx < len(lines):
        if lines[idx] == end_marker:
            found_end = True
            idx += 1
            break
        payload_lines.append(lines[idx])
        idx += 1
    if not found_end:
        raise SystemExit("Missing ciphertext END marker in vault file.")
    if any(l.strip() != "" for l in lines[idx:]):
        raise SystemExit("Unexpected trailing content after END marker in vault file.")
    ct_b64_flat = "".join(payload_lines)
    if not ct_b64_flat:
        raise SystemExit("Empty ciphertext payload in vault file.")
    try:
        ct_bytes = base64.b64decode(ct_b64_flat, validate=True)
    except (binascii.Error, ValueError) as exc:
        raise SystemExit(f"Corrupted base64 ciphertext payload: {exc}")
    if len(ct_bytes) < 16 or not ct_bytes.startswith(b"Salted__"):
        raise SystemExit("Ciphertext payload does not contain a valid OpenSSL salted stream.")

    if check_mac == "1":
        with os.fdopen(3, "rb") as pf:
            passphrase = pf.read()
        if not passphrase:
            raise SystemExit("Empty passphrase.")
        mac_key = hashlib.pbkdf2_hmac(
            "sha256",
            passphrase,
            b"ohc-env-vault-mac-v1:" + bytes.fromhex(mac_salt_hex),
            iterations,
            dklen=32,
        )
        canonical = (
            f"{expected_magic}\n"
            f"backend={backend_openssl}\n"
            f"kdf={expected_kdf}\n"
            f"iterations={iterations}\n"
            f"mac_salt={mac_salt_hex}\n"
            f"ciphertext_b64={ct_b64_flat}\n"
        ).encode("ascii")
        expected_mac = hmac.new(mac_key, canonical, hashlib.sha256).hexdigest()
        if not hmac.compare_digest(expected_mac, mac_hex):
            raise SystemExit("Authentication failed: wrong passphrase or tampered vault ciphertext.")

    if extract_ct:
        with open(extract_ct, "wb") as out:
            out.write(ct_bytes)
    print(f"{backend}\t{iterations}")
elif backend == backend_age:
    if begin_marker != "-----BEGIN AGE ENCRYPTED FILE-----":
        raise SystemExit("Unexpected BEGIN marker for age backend.")
    end_marker = "-----END AGE ENCRYPTED FILE-----"
    payload_lines = [begin_marker]
    found_end = False
    while idx < len(lines):
        payload_lines.append(lines[idx])
        if lines[idx] == end_marker:
            found_end = True
            idx += 1
            break
        idx += 1
    if not found_end:
        raise SystemExit("Missing AGE END marker in vault file.")
    if any(l.strip() != "" for l in lines[idx:]):
        raise SystemExit("Unexpected trailing content after AGE END marker.")
    if extract_ct:
        with open(extract_ct, "wb") as out:
            out.write(("\n".join(payload_lines) + "\n").encode("ascii"))
    print(f"{backend}\t0")
else:
    raise SystemExit(f"Unsupported vault backend '{backend}'.")
PY
}

age_encrypt_to_file() {
  local src_plain="$1"
  local dest_vault="$2"
  require_cmd age
  ensure_temp_dir

  local armor_tmp="${TEMP_DIR}/age.armor"
  if [[ -n "${OHC_ENV_AGE_RECIPIENT:-}" ]]; then
    age --armor -r "${OHC_ENV_AGE_RECIPIENT}" -o "${armor_tmp}" "${src_plain}"
  elif [[ -n "${KEY_FILE}" ]]; then
    validate_key_file "age identity key file"
    age --armor -e -i "${KEY_FILE}" -o "${armor_tmp}" "${src_plain}"
  else
    age --armor --passphrase -o "${armor_tmp}" "${src_plain}"
  fi

  local dest_dir
  dest_dir="$(dirname "${dest_vault}")"
  mkdir -p "${dest_dir}"
  local tmp_vault
  tmp_vault="$(mktemp "${dest_dir}/.env.enc.tmp.XXXXXX")"
  {
    printf '%s\n' "${VAULT_MAGIC}"
    printf 'backend=%s\n' "${BACKEND_AGE}"
    cat "${armor_tmp}"
  } > "${tmp_vault}"
  chmod 644 "${tmp_vault}"
  mv -f "${tmp_vault}" "${dest_vault}"
}

decrypt_vault_to_target() {
  local src_vault="$1"
  local dest_plain="$2"
  local pass_file="${3:-}"

  ensure_temp_dir
  local meta_info
  meta_info="$(validate_vault_envelope "${src_vault}" "0")"
  local detected_backend iterations
  IFS=$'\t' read -r detected_backend iterations <<<"${meta_info}"

  local dest_dir
  dest_dir="$(dirname "${dest_plain}")"
  mkdir -p "${dest_dir}"
  local tmp_plain
  tmp_plain="$(mktemp "${dest_dir}/.env.decrypt.tmp.XXXXXX")"
  chmod 600 "${tmp_plain}"
  STAGING_TEMP_FILE="${tmp_plain}"
  # Track temp file in case decrypt fails before rename
  local cleanup_tmp_plain="${tmp_plain}"

  if [[ "${detected_backend}" == "${BACKEND_OPENSSL}" ]]; then
    require_cmd openssl
    if [[ -z "${pass_file}" ]]; then
      pass_file="$(acquire_passphrase_file 0)"
    fi
    local ct_raw="${TEMP_DIR}/decrypt_cipher.bin"
    if ! validate_vault_envelope "${src_vault}" "1" "${pass_file}" "${ct_raw}" >/dev/null; then
      rm -f "${cleanup_tmp_plain}"
      exit 1
    fi
    if ! openssl enc -d -aes-256-cbc -pbkdf2 -iter "${iterations}" -md sha256 \
      -pass fd:3 -in "${ct_raw}" -out "${tmp_plain}" 3<"${pass_file}" 2>/dev/null; then
      rm -f "${cleanup_tmp_plain}"
      die "Decryption failed after authentication check; existing .env was not modified."
    fi
  elif [[ "${detected_backend}" == "${BACKEND_AGE}" ]]; then
    require_cmd age
    local armor_tmp="${TEMP_DIR}/decrypt_age.armor"
    if ! validate_vault_envelope "${src_vault}" "0" "" "${armor_tmp}" >/dev/null; then
      rm -f "${cleanup_tmp_plain}"
      exit 1
    fi
    if [[ -n "${KEY_FILE}" ]]; then
      validate_key_file "age identity key file"
      if ! age -d -i "${KEY_FILE}" -o "${tmp_plain}" "${armor_tmp}" 2>/dev/null; then
        rm -f "${cleanup_tmp_plain}"
        die "Age decryption failed; existing .env was not modified."
      fi
    else
      if ! age -d -o "${tmp_plain}" "${armor_tmp}"; then
        rm -f "${cleanup_tmp_plain}"
        die "Age decryption failed; existing .env was not modified."
      fi
    fi
  else
    rm -f "${cleanup_tmp_plain}"
    die "Unsupported vault backend: ${detected_backend}"
  fi

  chmod 600 "${tmp_plain}"
  mv -f "${tmp_plain}" "${dest_plain}"
  STAGING_TEMP_FILE=""
  chmod 600 "${dest_plain}"
}

cmd_store() {
  local key=""
  local has_value=0
  local read_stdin=0
  local value_arg=""

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --stdin)
        read_stdin=1
        shift
        ;;
      *)
        if [[ -z "${key}" ]]; then
          key="$1"
        elif [[ "${has_value}" == "0" ]]; then
          value_arg="$1"
          has_value=1
        else
          die "Unexpected extra argument for store: $1"
        fi
        shift
        ;;
    esac
  done

  [[ -n "${key}" ]] || die "Usage: env-vault.sh store <KEY> [VALUE | --stdin]"
  [[ "${key}" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || die "Invalid environment variable name '${key}' (must match ^[A-Za-z_][A-Za-z0-9_]*$)."

  assert_not_symlink "${ENV_FILE}" ".env"
  check_git_protection "${ENV_FILE}" ".env"

  ensure_temp_dir
  local val_file="${TEMP_DIR}/secret_val"
  if [[ "${read_stdin}" == "1" || ( "${has_value}" == "0" && ! -t 0 ) ]]; then
    python3 -c '
import sys
data = sys.stdin.buffer.read()
if data.endswith(b"\r\n"):
    data = data[:-2]
elif data.endswith(b"\n"):
    data = data[:-1]
with open(sys.argv[1], "wb") as out:
    out.write(data)
' "${val_file}"
  elif [[ "${has_value}" == "1" ]]; then
    OHC_VAULT_VALUE="${value_arg}" python3 -c '
import os, sys
with open(sys.argv[1], "wb") as out:
    out.write(os.environ["OHC_VAULT_VALUE"].encode("utf-8"))
' "${val_file}"
    unset value_arg
  elif [[ -r /dev/tty && -w /dev/tty ]]; then
    local prompt_val=""
    printf 'Enter value for %s: ' "${key}" >/dev/tty
    IFS= read -r -s prompt_val </dev/tty
    printf '\n' >/dev/tty
    OHC_VAULT_VALUE="${prompt_val}" python3 -c '
import os, sys
with open(sys.argv[1], "wb") as out:
    out.write(os.environ["OHC_VAULT_VALUE"].encode("utf-8"))
' "${val_file}"
    unset prompt_val
  else
    die "No value provided for '${key}'. Pass VALUE, --stdin, or run in an interactive TTY."
  fi
  chmod 600 "${val_file}"

  local env_dir
  env_dir="$(dirname "${ENV_FILE}")"
  mkdir -p "${env_dir}"
  local tmp_env
  tmp_env="$(mktemp "${env_dir}/.env.tmp.XXXXXX")"
  chmod 600 "${tmp_env}"

  python3 - "${ENV_FILE}" "${tmp_env}" "${key}" "${val_file}" <<'PY'
import os
import re
import sys

env_path, tmp_path, key, val_path = sys.argv[1:5]
with open(val_path, "rb") as vf:
    value = vf.read().decode("utf-8")

def format_env_value(val: str) -> str:
    if val != "" and re.fullmatch(r"[A-Za-z0-9_./:@+-]+", val):
        return val
    escaped = (
        val.replace("\\", "\\\\")
        .replace('"', '\\"')
        .replace("$", "\\$")
        .replace("`", "\\`")
        .replace("\r", "\\r")
        .replace("\n", "\\n")
    )
    return f'"{escaped}"'

new_line = f"{key}={format_env_value(value)}\n"
key_pattern = re.compile(rf"^\s*(?:export\s+)?{re.escape(key)}\s*=")

lines = []
if os.path.exists(env_path):
    with open(env_path, "r", encoding="utf-8") as ef:
        lines = ef.readlines()

updated = False
out_lines = []
for line in lines:
    if not updated and key_pattern.match(line):
        out_lines.append(new_line)
        updated = True
    elif key_pattern.match(line):
        # Drop duplicate definitions of the same key so the file stays canonical.
        continue
    else:
        out_lines.append(line)

if not updated:
    if out_lines and not out_lines[-1].endswith("\n"):
        out_lines[-1] += "\n"
    out_lines.append(new_line)

with open(tmp_path, "w", encoding="utf-8") as tf:
    tf.writelines(out_lines)
PY

  chmod 600 "${tmp_env}"
  mv -f "${tmp_env}" "${ENV_FILE}"
  chmod 600 "${ENV_FILE}"
  printf 'Stored key %s in %s (mode 0600).\n' "${key}" "${ENV_FILE}"
}

cmd_lock_or_encrypt() {
  local default_remove="$1"
  shift
  local remove_plaintext="${default_remove}"

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --remove-plaintext)
        remove_plaintext="1"
        shift
        ;;
      --keep-plaintext)
        remove_plaintext="0"
        shift
        ;;
      *)
        die "Unknown option for lock/encrypt: $1"
        ;;
    esac
  done

  assert_not_symlink "${ENV_FILE}" ".env"
  assert_not_symlink "${VAULT_FILE}" ".env.enc"
  [[ -f "${ENV_FILE}" ]] || die "Plaintext env file not found: ${ENV_FILE}"
  check_git_protection "${ENV_FILE}" ".env"

  local mode
  mode="$(get_file_mode "${ENV_FILE}")"
  if [[ "${mode:2:2}" != "00" ]]; then
    chmod 600 "${ENV_FILE}"
  fi

  ensure_temp_dir
  local pass_file=""
  if [[ "${BACKEND}" == "openssl" ]]; then
    pass_file="$(acquire_passphrase_file 1)"
    openssl_encrypt_to_file "${ENV_FILE}" "${VAULT_FILE}" "${pass_file}"
  elif [[ "${BACKEND}" == "age" ]]; then
    age_encrypt_to_file "${ENV_FILE}" "${VAULT_FILE}"
  else
    die "Unsupported backend '${BACKEND}' (expected 'openssl' or 'age')."
  fi

  # Always verify round-trip decrypt matches original plaintext byte-for-byte before touching plaintext.
  local rt_plain="${TEMP_DIR}/roundtrip.env"
  if [[ "${BACKEND}" == "openssl" || -n "${KEY_FILE}" ]]; then
    decrypt_vault_to_target "${VAULT_FILE}" "${rt_plain}" "${pass_file}"
    if ! cmp -s "${ENV_FILE}" "${rt_plain}"; then
      die "Round-trip verification mismatch; plaintext ${ENV_FILE} was preserved."
    fi
  else
    validate_vault_envelope "${VAULT_FILE}" "0" >/dev/null
  fi

  if [[ "${remove_plaintext}" == "1" ]]; then
    rm -f "${ENV_FILE}"
    printf 'Locked %s -> %s (backend=%s, round-trip verified, plaintext removed).\n' \
      "${ENV_FILE}" "${VAULT_FILE}" "${BACKEND}"
  else
    printf 'Encrypted %s -> %s (backend=%s, round-trip verified, plaintext kept).\n' \
      "${ENV_FILE}" "${VAULT_FILE}" "${BACKEND}"
  fi
}

cmd_unlock() {
  if [[ $# -gt 0 ]]; then
    die "Unknown option for unlock/decrypt: $1"
  fi
  assert_not_symlink "${ENV_FILE}" ".env"
  assert_not_symlink "${VAULT_FILE}" ".env.enc"
  [[ -f "${VAULT_FILE}" ]] || die "Encrypted vault file not found: ${VAULT_FILE}"
  check_git_protection "${ENV_FILE}" ".env"

  decrypt_vault_to_target "${VAULT_FILE}" "${ENV_FILE}"
  printf 'Unlocked %s -> %s (mode 0600).\n' "${VAULT_FILE}" "${ENV_FILE}"
}

cmd_status_or_verify() {
  local mode_name="$1"
  shift
  local check_mac="0"
  local require_vault="0"

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --check-mac)
        check_mac="1"
        shift
        ;;
      --require-vault)
        require_vault="1"
        shift
        ;;
      *)
        die "Unknown option for ${mode_name}: $1"
        ;;
    esac
  done

  local errors=0

  printf 'repo_root=%s\n' "${REPO_ROOT}"
  printf 'env_file=%s\n' "${ENV_FILE}"
  printf 'vault_file=%s\n' "${VAULT_FILE}"

  if [[ -L "${ENV_FILE}" ]]; then
    printf 'env_symlink=FAIL (symlink refused)\n' >&2
    errors=$((errors + 1))
  fi
  if [[ -L "${VAULT_FILE}" ]]; then
    printf 'vault_symlink=FAIL (symlink refused)\n' >&2
    errors=$((errors + 1))
  fi

  local info=""
  if info="$(git_rel_path_for "${ENV_FILE}")"; then
    local git_root rel_path
    IFS=$'\t' read -r git_root rel_path <<<"${info}"
    if git -C "${git_root}" ls-files --error-unmatch -- "${rel_path}" >/dev/null 2>&1; then
      printf 'git_tracked=FAIL (%s is tracked in git)\n' "${rel_path}" >&2
      errors=$((errors + 1))
    else
      printf 'git_tracked=ok (untracked)\n'
    fi
    if git -C "${git_root}" check-ignore -q -- "${rel_path}"; then
      printf 'git_ignored=ok\n'
    else
      printf 'git_ignored=FAIL (%s is not ignored by .gitignore)\n' "${rel_path}" >&2
      errors=$((errors + 1))
    fi
  else
    printf 'git_protection=outside-git-repo\n'
  fi

  local has_plain=0 has_vault=0
  if [[ -f "${ENV_FILE}" && ! -L "${ENV_FILE}" ]]; then
    has_plain=1
    local perm
    perm="$(get_file_mode "${ENV_FILE}")"
    if [[ "${perm}" != "0600" ]]; then
      printf 'env_permissions=FAIL (mode %s, expected 0600)\n' "${perm}" >&2
      errors=$((errors + 1))
    else
      printf 'env_permissions=ok (0600)\n'
    fi
  else
    printf 'env_plaintext=absent\n'
  fi

  if [[ -f "${VAULT_FILE}" && ! -L "${VAULT_FILE}" ]]; then
    has_vault=1
    local meta_out=""
    if meta_out="$(validate_vault_envelope "${VAULT_FILE}" "0" 2>&1)"; then
      local detected_backend iterations
      IFS=$'\t' read -r detected_backend iterations <<<"${meta_out}"
      printf 'vault_envelope=ok (backend=%s, iterations=%s)\n' "${detected_backend}" "${iterations}"
      if [[ "${mode_name}" == "verify" && "${detected_backend}" == "${BACKEND_OPENSSL}" ]]; then
        if [[ "${check_mac}" == "1" || -n "${OHC_ENV_PASSPHRASE:-}" || -n "${KEY_FILE}" ]]; then
          local pass_file
          pass_file="$(acquire_passphrase_file 0)"
          if validate_vault_envelope "${VAULT_FILE}" "1" "${pass_file}" >/dev/null 2>&1; then
            printf 'vault_mac=ok\n'
          else
            printf 'vault_mac=FAIL (authentication failed)\n' >&2
            errors=$((errors + 1))
          fi
        fi
      fi
    else
      printf 'vault_envelope=FAIL (%s)\n' "${meta_out}" >&2
      errors=$((errors + 1))
    fi
  else
    printf 'vault_envelope=absent\n'
    if [[ "${require_vault}" == "1" ]]; then
      printf 'vault_required=FAIL (missing %s)\n' "${VAULT_FILE}" >&2
      errors=$((errors + 1))
    fi
  fi

  if [[ "${has_plain}" == "1" ]]; then
    printf 'state=unlocked\n'
  elif [[ "${has_vault}" == "1" ]]; then
    printf 'state=locked\n'
  else
    printf 'state=uninitialized\n'
  fi

  if [[ "${errors}" -gt 0 ]]; then
    exit 1
  fi
}

cmd_keys() {
  assert_not_symlink "${ENV_FILE}" ".env"
  [[ -f "${ENV_FILE}" ]] || die "Plaintext env file not found: ${ENV_FILE}"
  python3 - "${ENV_FILE}" <<'PY'
import re, sys
pat = re.compile(r"^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=")
with open(sys.argv[1], "r", encoding="utf-8") as f:
    for line in f:
        m = pat.match(line)
        if m:
            print(m.group(1))
PY
}

main() {
  local cmd=""
  local cmd_args=()

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --repo-root)
        [[ $# -ge 2 ]] || die "--repo-root requires a directory argument."
        REPO_ROOT="$2"
        shift 2
        ;;
      --env-file)
        [[ $# -ge 2 ]] || die "--env-file requires a path argument."
        ENV_FILE="$2"
        shift 2
        ;;
      --vault-file)
        [[ $# -ge 2 ]] || die "--vault-file requires a path argument."
        VAULT_FILE="$2"
        shift 2
        ;;
      --key-file)
        [[ $# -ge 2 ]] || die "--key-file requires a path argument."
        KEY_FILE="$2"
        shift 2
        ;;
      --backend)
        [[ $# -ge 2 ]] || die "--backend requires 'openssl' or 'age'."
        BACKEND="$2"
        shift 2
        ;;
      -h|--help|help)
        usage
        exit 0
        ;;
      *)
        if [[ -z "${cmd}" ]]; then
          cmd="$1"
        else
          cmd_args+=("$1")
        fi
        shift
        ;;
    esac
  done

  [[ -n "${cmd}" ]] || { usage >&2; exit 1; }
  resolve_paths

  case "${cmd}" in
    store|set)
      cmd_store "${cmd_args[@]}"
      ;;
    lock)
      cmd_lock_or_encrypt "1" "${cmd_args[@]}"
      ;;
    encrypt)
      cmd_lock_or_encrypt "0" "${cmd_args[@]}"
      ;;
    unlock|decrypt)
      cmd_unlock "${cmd_args[@]}"
      ;;
    status)
      cmd_status_or_verify "status" "${cmd_args[@]}"
      ;;
    verify)
      cmd_status_or_verify "verify" "${cmd_args[@]}"
      ;;
    keys)
      cmd_keys "${cmd_args[@]}"
      ;;
    *)
      die "Unknown command '${cmd}'. Run 'scripts/env-vault.sh --help' for usage."
      ;;
  esac
}

main "$@"
