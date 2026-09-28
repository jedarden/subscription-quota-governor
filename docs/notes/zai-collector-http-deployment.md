# Securely deploying a Z.AI collector over `normalized_http`

This restates
[the plan's Claude Code with Z.AI section](../plan/plan.md#73-claude-code-with-zai)
(§7.3) as an operator deployment guide. It covers the `normalized_http`
transport specifically; see
[configuration notes](configuration.md#normalized_http) for the source types
in general.

## The constraint this works around

There is no private endpoint or deployment component for Z.AI in this
repository (§7.3) -- a site-local collector is something you run yourself,
emitting the normalized quota contract. `normalized_http`'s configuration
shape (`src/config.rs`) is exactly:

```yaml
source:
  type: normalized_http
  url: "..."
  timeout_seconds: 10   # optional, defaults to 10
```

There is no field for a header, bearer token, API key, or client certificate
-- not "no *secret* header," literally no header configuration exists at
all. This is intentional (§14 requirement 3, "generic HTTP does not accept
plaintext credential configuration") and permanent for v1, not a gap to work
around by finding an undocumented field. **The collector endpoint's access
control has to live entirely at the network layer**, because `subgov` will
never present a credential to it.

Two other constraints shape deployment, both enforced in
`src/source.rs::http_agent` / `read_generic_http_bytes`:

- **Redirects are always refused**, not followed -- even same-origin. A 3xx
  response is treated as a hard error before any JSON parsing happens. If
  the collector sits behind something that redirects HTTP to HTTPS, or
  redirects on a trailing slash, point `url` at the final destination
  directly; do not rely on the collector to redirect `subgov` there.
- **The response body is capped at 1 MiB** (`MAX_GENERIC_SOURCE_BYTES`,
  shared with every other generic source). The normalized quota contract is
  tiny, so this is not a practical limit for a correct collector, but a
  collector that echoes debug payloads or verbose errors into the response
  body can trip it.
- **`timeout_seconds` must be greater than zero** -- a `0` value fails
  config validation before any request is attempted, rather than producing
  a request with no effective deadline.

## Deployment options, in preference order

### 1. Prefer `command` or `normalized_file` when you can

If the collector's job is "run a script that talks to Z.AI and prints JSON,"
`command` needs no network listener at all -- see
[the `claude-zai.yaml` example](../../examples/claude-zai.yaml), which is
the currently-documented approach and requires none of the access-control
work below. If instead you want a long-running daemon that polls Z.AI on its
own schedule (e.g. to smooth out Z.AI's own rate limits independently of
`subgov`'s poll cycle) and hands off the latest snapshot, prefer having it
write atomically to a local file that `subgov` reads with `normalized_file`
over standing up an HTTP listener. A file has no network surface to secure
at all -- correctness is just ordinary filesystem permissions (owner-only
read, same pattern as `governor-state.json`'s `0600` in
[state backup and removal](state-backup-and-removal.md)) plus an atomic
write so `subgov` never reads a half-written file.

`normalized_http` is for the case those two don't fit: the collector is a
daemon, it is not (or cannot be) colocated with `subgov` on the same
filesystem, or a file/command hand-off is otherwise impractical for your
setup.

### 2. Loopback-only, same host (the baseline `normalized_http` deployment)

Run the collector daemon and `subgov` on the same host. Bind the collector
strictly to `127.0.0.1` (never `0.0.0.0` or a routable interface), and point
`subgov` at `http://127.0.0.1:<port>/...`:

```yaml
source:
  type: normalized_http
  url: "http://127.0.0.1:8791/quota/claude-zai"
  timeout_seconds: 10
```

This is the closest HTTP equivalent to a `command` source: traffic never
leaves the host's loopback interface, so there is no cross-host eavesdropper
or spoofer to defend against, and plaintext `http://` is fine here -- TLS
would protect a network hop that does not exist. Verify the bind address
after any collector restart (`ss -ltnp | grep <port>`), since a collector
config change that widens the bind address is a silent downgrade of this
guarantee.

### 3. Tailnet-restricted, cross-host

If the collector cannot be colocated, bind it to the host's Tailscale
interface address (never a public interface) and restrict which tailnet
peers may reach that port with a `tag:`-scoped Tailscale ACL rule naming the
specific device(s) running `subgov` -- not `autogroup:admin` and not a broad
tag, since ACL membership is the *entire* access control here. Use
`https://` with a certificate that Rust's default TLS trust store accepts
(a certificate from a real CA, e.g. via a reverse proxy such as Caddy or
Traefik in front of the collector); `subgov`'s HTTP client (`ureq`, via
`src/source.rs::http_agent`) has no way to pin or otherwise trust a
self-signed certificate, so a self-signed cert here just fails every poll
rather than degrading gracefully.

This is the "otherwise access-controlled" alternative to loopback that §7.3
allows, but it is strictly weaker: anyone who can reach the port and is
permitted by the ACL can read the normalized quota snapshot (account name
in the URL path, usage fractions, reset timestamps, worker counts) with no
further check, because `subgov` has nothing to present as a credential.
Per [state backup and removal](state-backup-and-removal.md)'s description of
the same data at rest, this is operational data rather than a secret --
still, prefer option 2 whenever colocation is possible, and never expose
this port on a public interface or an unrestricted tailnet ACL rule.

### 4. What not to do

- Do not put a Z.AI API key or session token in the `url` as a query
  parameter (`?api_key=...`) to work around the lack of a header field --
  that value lands in `subgov`'s own logs and error context on any
  malformed-response path exactly like any other URL content, and query
  strings are commonly logged by reverse proxies and access logs on the
  collector side too. Give the collector itself Z.AI credentials through its
  own environment or credential store, entirely on the collector side of the
  boundary `subgov` never crosses.
- Do not bind the collector to a public or unrestricted interface "just for
  now" -- there is no request-level authentication to fall back on if the
  network boundary is misconfigured, unlike a typical API that would at
  least reject an unauthenticated caller.
- Do not rely on an HTTP redirect (e.g. a load balancer doing TLS
  termination and redirecting to a backend) -- `subgov` refuses every 3xx
  response outright (see above).
