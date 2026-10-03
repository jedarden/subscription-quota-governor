# Configuration compatibility policy

This restates the compatibility rules from
[the plan's configuration contract](../plan/plan.md#8-configuration-contract)
(§8) for operators who need the policy without reading the whole plan.

## Two independent version numbers

- The top-level YAML `version: 1` field is the **config schema** version.
  `subgov` rejects any file whose `version` it does not implement exactly
  (`src/config.rs` fails closed on a mismatch) -- there is no partial or
  best-effort parsing of an unsupported schema version.
- The `subgov` binary itself follows [Semantic Versioning](https://semver.org/)
  (see [CHANGELOG.md](../../CHANGELOG.md)). A binary release can add features
  or fix bugs within the same config schema version; a schema-version bump is
  reserved for changes that would otherwise silently break an existing file's
  meaning.

A new binary release does not imply a new config `version:`, and a new config
`version:` does not by itself imply a new major binary release.

## v1.0.0-rc.1

This first release candidate accepts configuration schema `version: 1` and
does not change the schema contract. The `-rc.1` suffix marks the binary as a
pre-release under Semantic Versioning; it does not identify a new config
schema. Use `run --observe-only` for this rollout.

## Backward-compatible within a schema version

- Adding a new **optional** field with a safe, conservative default.
- Adding a new accepted value to an existing field, provided every value that
  was previously accepted keeps its previous meaning.

## Requires a new schema version

- Renaming an existing field.
- Changing the meaning or default of an existing field.
- Making a previously-rejected shape valid in a way that would change the
  behavior of an existing file that happens to match it.

`subgov` uses strict unknown-field rejection (`#[serde(deny_unknown_fields)]`)
on every config struct, so a typo, or a field meant for a newer schema
version, fails validation immediately instead of being silently ignored.

## Deprecation

A field slated for removal stays functional, with a deprecation warning, for
at least one minor release before it is deleted. Deleting it is itself a
schema-version bump whenever a config file that still sets that field would
otherwise change behavior once the field disappears.

## Checking a config against this policy

```console
subgov --config governor.yaml check
```

`check` validates the file -- schema version, unknown fields, and every
[validation rule](../plan/plan.md#8-configuration-contract) -- without
contacting any quota source or fleet observer/actuator. It is safe to run
against a config you have not deployed yet, including in CI.
