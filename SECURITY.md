# Security Policy

## Supported Versions

Security fixes are maintained for the current minor release line of OneHumanCorp
Community (Single-Tenant) Edition and the `main` branch.

| Version | Supported |
| --- | --- |
| Current `0.4.x` / `main` | Yes |
| Older releases | Upgrade to latest release |

## Reporting a Vulnerability

**Please do not report security vulnerabilities through public GitHub issues,
discussions, or pull requests.**

Instead, report vulnerabilities confidentially using either:

1. **GitHub Private Vulnerability Reporting:** Use the *"Report a
   vulnerability"* button under the repository's **Security** tab.
2. **Email:** Send an encrypted or detailed report to
   `security@onehumancorp.com`.

### What to Include

To help us triage and remediate issues quickly, please include:

- Affected version, release tag, or commit SHA.
- Component or surface affected (e.g., server API, `ConnectionVault`, auth,
  SQLCipher storage, agent harness/MCP sandbox, web UI, Tauri shell, CLI).
- Clear step-by-step reproduction instructions or a minimal proof of concept.
- Assessment of potential security impact and any known workaround or fix.

## Response & Disclosure Process

- **Acknowledgment:** We aim to acknowledge receipt within **3 business days**.
- **Triage & Remediation:** We will investigate, validate severity, keep you
  updated on fix progress, and coordinate a disclosure timeline once a patch is
  available.
- **Credit:** With your permission, we are happy to credit reporters in the
  published security advisory and release notes.

## High-Priority Security Boundaries

We especially welcome reports affecting:

- Server-side standing authority, approval gates, and spending budget controls.
- Credential encryption, scoping, and revocation in `ConnectionVault`.
- Authentication, session handling, and webhook signature verification.
- Agent execution harness sandboxing, command execution, and MCP tool
  boundaries.
- Local SQLCipher database encryption and desktop runtime origin isolation.
