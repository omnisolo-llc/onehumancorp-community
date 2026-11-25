# OneHumanCorp Edition Comparison

OneHumanCorp is available in two editions built on the same core solo-business operating system:

- **OneHumanCorp Community Edition (`Apache-2.0`)**: Complete, single-tenant, self-hosted business operations platform for solo business owners — zero artificial feature paywalls.
- **OneHumanCorp Cloud Edition (Proprietary, Hosted by OmniSolo LLC)**: Managed multi-tenant SaaS infrastructure with enterprise identity, hosted scaling, cross-device cloud sync, and platform operations.

---

## Side-by-Side Overview

| Capability Area | OneHumanCorp Community (`Apache-2.0`) | OneHumanCorp Cloud (Proprietary) |
| --- | --- | --- |
| **License** | Apache License 2.0 (Open Source) | Proprietary (All Rights Reserved) |
| **Hosting & Topology** | Self-hosted single-tenant (Tauri desktop app, single-machine Docker Compose, or personal VPS) | Managed multi-tenant SaaS hosted and operated by OmniSolo LLC |
| **Solo-Owner Business Suite** | **Full function**: proposals, quotes, invoicing, client booking & calendar scheduling, customer product subscriptions, catalog, orders, POS/terminal, fulfillment, shipping, storefront/website builder, CRM, unified inbox, customer memory graph, and growth widgets | Full solo-owner business suite included |
| **Omni Assistant Task Center** | Included (conversational task management, Vibe Spaces, approvals, and workflow visibility) | Included |
| **Standing Authority & Approvals** | Included (server-enforced standing authority rules and owner approval feed) | Included |
| **AI Agents & Harnesses** | Included (built-in agent runtime, skills, inbound/outbound MCP tooling, portable session capsules, multi-harness workers) | Included, plus managed multi-tenant autoscaled worker pools |
| **LLM Providers & Local Inference** | Bring Your Own Key (OpenAI, Anthropic, MiniMax, Gemini, OpenAI-compatible endpoints) and local open-weights inference via **Ollama** | BYOK plus optional hosted managed model routing and credit pools |
| **Cost Visibility & Hard Spending Caps** | Local micro-USD `UsageLedger` and owner-configured hard spending limits for your own BYOK API spend | Local `UsageLedger` plus multi-tenant platform metering, hosted plan quotas, and SaaS billing |
| **Merchant Payments (Your Customers)** | **Included**: connect your own Stripe account via `ConnectionVault` to collect proposal deposits, invoices, bookings, storefront checkouts, and customer subscriptions | Included |
| **Platform SaaS Billing & Tiers** | Not applicable (self-hosted, no platform paywalls or tier limits) | Managed subscription tiers, prepaid credit wallets, and Stripe platform billing |
| **Primary Database** | Encrypted **SQLCipher SQLite** by default (zero external dependencies) + optional self-hosted single-tenant **PostgreSQL** | Managed high-availability PostgreSQL clusters with enforced row-level tenant isolation |
| **Caching & Realtime Transport** | In-process Tokio channels and local transactional locks (zero external cache server required) | Distributed cache clusters, pub/sub fanout, and cross-pod distributed leases |
| **Offline & Cross-Device Sync** | Local-first offline POS/client queue and single-node persistence | Managed real-time multi-device cloud synchronization |
| **Authentication & Identity** | Local owner/staff email + password JWT sessions, API keys, local `.env` secret vault | Enterprise SSO / OIDC identity federation, SCIM provisioning, and cryptographic workload mTLS identity |
| **Cross-Tenant Collectives & Syndication** | Not included (strict single-tenant boundary) | Multi-tenant collectives, discovery, and catalog syndication |
| **Sandboxed Agent Execution** | Local process and OS-level workspace sandboxing | Isolated multi-tenant microVM / user-space kernel sandboxes with strict egress controls |
| **Operations, Observability & Support** | Self-managed Docker Compose / native binaries / desktop app; community GitHub Discussions | Managed Kubernetes orchestration, automated backups, fleet dashboards, alerting, and commercial SLA support |

---

## Core Open-Core Principle: Full Single-Tenant Parity

OneHumanCorp Community is designed so an independent business owner running on a laptop, desktop, or personal server can operate their entire business without hitting feature paywalls:

1. **Merchant Payments Are Always Open Source.** Collecting payments from *your own customers* (proposal deposits, invoices, bookings, orders, and your own customer subscriptions) using your own Stripe credentials is part of the Apache-2.0 Community Edition. Only OmniSolo's own multi-tenant SaaS billing and hosted tier enforcement remain proprietary to OneHumanCorp Cloud.
2. **Local Cost Control Without Cloud Lock-In.** Community Edition includes the full micro-USD `UsageLedger`, `ConnectionVault` credential encryption, and hard spending limits so you stay in control of your direct LLM provider spend.
3. **Zero Cloud Infrastructure Overhead.** Community Edition runs out of the box with encrypted SQLCipher SQLite — no distributed cache cluster, no Kubernetes cluster, and no external sync service required.

---

## Which Edition Should I Choose?

- **Choose OneHumanCorp Community (`Apache-2.0`)** if you are a solo founder, consultant, creator, or developer who wants full ownership of your data on your own machine or VPS, prefers BYOK or local Ollama inference, and wants to inspect, customize, or extend the codebase under Apache-2.0.
- **Choose OneHumanCorp Cloud** if you want zero-ops managed hosting, automatic high-availability database management and backups, multi-device real-time cloud sync, enterprise SSO/OIDC, isolated cloud microVM execution, and commercial support.

---

## Licensing & Trademarks

- The Community Edition source code is licensed under the [Apache License, Version 2.0](../../LICENSE).
- OneHumanCorp Cloud multi-tenant infrastructure, platform billing, enterprise SSO/OIDC orchestration, and hosted deployment automation are proprietary to OmniSolo LLC (All Rights Reserved).
- Under Section 6 of the Apache License 2.0, the license grants no rights to use the "OneHumanCorp" or "OmniSolo" trademarks in modified distributions or competing hosted services.
