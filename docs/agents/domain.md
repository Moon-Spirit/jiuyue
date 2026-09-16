# Domain Docs

How the engineering skills should consume this repo's domain documentation when exploring the codebase.

## Before exploring, read these

- **`CONTEXT.md`** at the repo root — the IM ubiquitous language (Conversation, Message, Sequence Number, Read Marker vs Read Receipt, Secret Chat, Presence).
- **`docs/adr/`** — read ADRs that touch the area you're about to work in.

If any of these files don't exist, **proceed silently**. Don't flag their absence; don't suggest creating them upfront. The `/domain-modeling` skill (reached via `/grill-with-docs` and `/improve-codebase-architecture`) creates them lazily when terms or decisions actually get resolved.

## File structure

This is a **single-context repo**. `frontend/`, `backend/` and `desktop/` are separate packages but share one ubiquitous language (the IM domain), so there is one glossary and one ADR log.

```
/
├── CONTEXT.md
├── AGENTS.md
├── docs/
│   ├── adr/                  ← system-wide architecture decisions
│   ├── agents/               ← skill configuration (this file's home)
│   └── research/             ← research findings that informed decisions
├── backend/                  ← Rust workspace (planned)
├── frontend/                 ← Vue 3 + Vite + TS (planned)
└── desktop/                  ← Tauri v2 shell (planned)
```

## Use the glossary's vocabulary

When your output names a domain concept (in an issue title, a refactor proposal, a hypothesis, a test name), use the term as defined in `CONTEXT.md`. Don't drift to synonyms the glossary explicitly avoids.

Two pairs are especially load-bearing — getting them wrong is a real bug, not a naming nit:

- **Read Marker** (private, per User, drives unread counts) vs **Read Receipt** (public, shown to others). Never mix them.
- **Sync Cursor** (per Device) vs **Read Marker** (per User).

If the concept you need isn't in the glossary yet, that's a signal — either you're inventing language the project doesn't use (reconsider) or there's a real gap (note it for `/domain-modeling`).

## Flag ADR conflicts

If your output contradicts an existing ADR, surface it explicitly rather than silently overriding:

> _Contradicts ADR-0005 (single-node start with million-scale plan) — but worth reopening because…_

The ADRs most likely to be relevant: 0001 (E2EE dual mode), 0003 (delivery protocol), 0004 (media plane independence).
