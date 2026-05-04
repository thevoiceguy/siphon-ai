# SiphonAI WebSocket Bridge Protocol — v1

**Status:** stub. The canonical spec lives here; until it is filled in, see
`DEV_PLAN.md` §4 for the working draft.

This document is the contract for third-party developers building WS servers
against SiphonAI. Treat it like a published API (CLAUDE.md §4.2).

## Versioning
Every breaking change bumps the `version` field on the `start` message.

## Frame types
TBD.

## Message reference (SiphonAI → server)
TBD.

## Message reference (server → SiphonAI)
TBD.

## Rules
- 20ms PCM16-LE mono audio frames. Always.
- `seq` numbers are monotonic per-call; never reset.
