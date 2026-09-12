# Provider quota surfaces

## Claude Code / Anthropic

The Claude Code credential file supplies OAuth access, refresh, and expiry
fields. The usage response exposes legacy named windows (`five_hour`,
`seven_day`, `weekly_scoped`) and may also expose a generic `limits[]` array.
The adapter treats only explicitly inactive limits as inactive and lets generic
limits supersede same-name legacy entries.

This surface is not presented as a stable public Anthropic API. Endpoint URLs
are configurable so an installation can adapt without changing controller
logic. Provider failures stop actuation for that account.

## Codex

The official Codex app-server protocol exposes `account/rateLimits/read` and
`account/rateLimits/updated`. A full read can contain a compatibility
`rateLimits` bucket and a canonical multi-bucket `rateLimitsByLimitId` map.
Primary and secondary windows report `usedPercent`, `windowDurationMins`, and a
Unix-second `resetsAt` value.

Reference: [Codex App Server documentation](https://developers.openai.com/codex/app-server).

## Claude Code / Z.AI

No stable public provider surface is assumed. A site-local collector supplies
the same normalized snapshot through the command, file, or HTTP adapter. This
keeps provider-specific endpoints, credentials, and deployment architecture out
of the governor and its configuration.

## Consolidation consequence

All provider shapes terminate at `QuotaSnapshot`. Policy never switches on a
provider name. This keeps source-specific authentication and parsing at the
edge while making reset handling, staleness, multi-window arbitration, state,
and fleet actuation shared and testable.
