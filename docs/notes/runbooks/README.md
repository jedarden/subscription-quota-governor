# Operator runbooks

Runbooks for conditions an operator needs to diagnose and act on in a running
deployment. Each restates the relevant plan.md section(s) as a
detect-diagnose-remediate-verify procedure, cited against the actual
implementation rather than the plan's WP6 checklist alone.

| Runbook | Covers |
| --- | --- |
| [Stale or failed sources](stale-sources.md) | A quota or resource source stops updating, or fails outright, for one or more accounts. |
| [Authentication expiry](authentication-expiry.md) | An expired, revoked, or missing Anthropic OAuth or Codex login. |

See also [state backup and removal](../state-backup-and-removal.md) and
[secure HTTP deployment for the Z.AI collector](../zai-collector-http-deployment.md),
which cover adjacent operational topics but aren't incident runbooks.
