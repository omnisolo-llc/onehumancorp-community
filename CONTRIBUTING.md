# Contributing to OneHumanCorp

Thank you for your interest in contributing to OneHumanCorp! This guide covers
our contribution terms, local development workflow, and verification standards.

## 1. License, DCO & Contributor Agreement

The OneHumanCorp Community (Single-Tenant) Edition is licensed under the
[Apache License, Version 2.0](LICENSE). Please also review our
[Trademark Policy](TRADEMARKS.md).

### Developer Certificate of Origin (DCO 1.1)

Every commit submitted to this repository must include a `Signed-off-by`
trailer certifying compliance with the
[Developer Certificate of Origin (DCO) 1.1](https://developercertificate.org/):

```text
Signed-off-by: Your Name <you@example.com>
```

You can add this trailer automatically when committing with Git:

```bash
git commit -s -m "feat(storage): add SQLite migration check"
```

### Inbound Contribution Terms (CLA)

In addition to DCO sign-off, external pull requests require accepting our
Contributor License Agreement (CLA) so OmniSolo LLC receives a perpetual,
worldwide, non-exclusive copyright and patent license to include accepted
contributions across both the Apache-2.0 Community Edition and OneHumanCorp
Cloud Edition.

## 2. Local Development Setup

### Prerequisites

- **Rust:** Stable toolchain matching `rust-toolchain.toml` (with `rustfmt` and
  `clippy` components).
- **Node.js:** Version matching `.node-version` (Node.js 22.x) and `npm`.
- See `docs/development/native-build.md` for optional desktop (Tauri) and
  browser (Playwright) prerequisites.

### Common Verification Commands

```bash
# Formatting, Clippy, ESLint, and TypeScript checks
make lint

# Focused backend & Node verification
make test-backend
make test-node
make test-contracts

# Full workspace acceptance suite (Rust + Node/Web/CLI + contracts + E2E)
make test
```

## 3. Engineering Standards

- **Real behavior over mocks:** Production code and tests must exercise real
  routers, real persistence (SQLCipher SQLite or PostgreSQL), and real local
  HTTP loopback servers rather than fake/stub doubles or synthetic fallbacks.
- **Modular changes:** Place new features in focused, single-responsibility
  modules rather than growing large legacy files inline.
- **Fail-closed configuration:** Missing credentials, invalid origins, or
  missing authority grants must return explicit errors rather than silent
  permissive defaults.
- **Dependency license hygiene:** New Rust crates and npm packages must use
  permissive or weak-copyleft SPDX licenses (`Apache-2.0`, `MIT`,
  `BSD-2-Clause`, `BSD-3-Clause`, `ISC`, `Zlib`, `MPL-2.0`). Strong-copyleft
  (`AGPL`, `GPL`) and source-available (`SSPL`, `BSL`) dependencies are not
  permitted (see `deny.toml`).
- **Web accessibility & performance:** Web UI changes must follow semantic
  HTML5, WCAG 2.2 AA accessibility, keyboard navigation, and self-hosted or
  system typography (no external font CDNs).

## 4. Pull Request Checklist

1. Rebase cleanly on `main` and sign off every commit (`git commit -s`).
2. Keep PRs scoped to one cohesive change with clear rationale and test output.
3. Verify `make lint` and relevant test suites pass locally with zero warnings.
4. For security vulnerabilities, **do not** open a public issue or PR — follow
   [SECURITY.md](SECURITY.md) instead.
