# SiphonAI Dialplan / Route Matching

**Status:** stub. See `DEV_PLAN.md` §6.3 for current matching semantics.

- Routes evaluate top-down; first match wins.
- All match keys within a route AND.
- `regex = true` is per-route, not per-match-key.
- Trailing default route (`any = true`) is required in production configs.
