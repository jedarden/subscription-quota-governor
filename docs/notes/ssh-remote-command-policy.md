# SSH remote-command policy for cross-host sources

Restates [the plan's remote transport section](../plan/plan.md#2210-remote-transport)
(§22.10) as a security-relevant configuration policy, cross-checked against
`src/config.rs` and `src/fleet.rs`.

## The sanctioned shape

A cross-host `command` source, observer, or actuator (used for the
multi-host resource-aware placement `hosts` configuration in §22.3) runs
over SSH using the **existing generic `command` primitive verbatim** --
there is no dedicated SSH transport type, no SSH-specific config fields,
and no code path in `subgov` that treats `ssh` as anything other than
ordinary `argv[0]`:

```yaml
argv: [ssh, <host>, <remote-argv...>]
```

For example (the plan's own two-host illustration, §22.3 -- one host
reached locally, one over SSH):

```yaml
hosts:
  lab:
    max_workers: 8
    resource_reserve:
      cpu_reserve_fraction: 0.30
      mem_reserve_mb: 8192
    resource_source:
      type: command
      argv: [ssh, lab.tailnet, resource-probe]
    observer:
      type: command
      argv: [ssh, lab.tailnet, needle-worker-count, claude-anthropic]
    actuator:
      type: command
      argv: [ssh, lab.tailnet, needle-set-target, claude-anthropic, "{desired_workers}"]
```

`ssh` itself is exec'd directly by `subgov`, the same way any other
`command` argv is (`src/fleet.rs`'s `CommandObserver`/`CommandActuator` and
`src/source.rs`'s generic command collector all build a `std::process::Command`
from the argv array with no shell involved -- per §7.4, "commands are argv
arrays and are never interpreted by a shell"). This holds for the local
leg of the call exactly as it does for a non-SSH command; nothing about
this local exec step changes when `argv[0]` happens to be `ssh`.

## Why this is a different injection boundary than a local command

For an ordinary local `command` source/observer/actuator, `subgov`'s own
argv-array (never-shell) execution is the entire injection-safety story --
there is no second parsing step downstream, so there is nothing further to
reason about (§7.4).

**SSH breaks that property.** The remote command portion of the argv array
(everything after `<host>`) is not executed as a literal argv on the remote
side the way it was invoked locally. `ssh` joins those trailing arguments
into a single string and hands it to the *remote user's login shell* to
parse and execute (this is standard OpenSSH behavior, not something
`subgov` controls or can disable through this generic `command` primitive).
That means **argv-level injection safety past the local `ssh` exec is the
remote script's responsibility, not `subgov`'s** -- `subgov`'s own
non-shell-exec guarantee protects the hop onto the SSH connection, not
what happens once the remote shell receives it.

Concretely, this is why the sanctioned pattern is:

- **Fixed, non-interpolated remote scripts only.** Every element after
  `<host>` in the example above (`needle-worker-count`, `claude-anthropic`,
  `resource-probe`) is a literal, known-in-advance token -- a script name
  and a small number of fixed identifiers from `subgov`'s own config, not
  data read from anywhere untrusted.
- **Never build remote argv from live provider data.** A resource or
  quota snapshot's fields (usage fractions, window IDs, provider-reported
  strings) must never be substituted into the remote argv, because they
  would then be parsed by the remote shell as shell syntax rather than as
  inert data -- unlike a local `command` invocation, where the argv-array
  discipline makes that class of injection structurally impossible
  regardless of what the data contains. `{desired_workers}` is the one
  substitution `subgov` performs (an integer it computed itself, never
  provider-controlled free text) -- see
  [configuration notes](configuration.md#fleet-integration) for the
  general `{desired_workers}` substitution contract this inherits.
- **Design the remote script to take fixed, positional identifiers** (an
  account name, a host name) exactly as the example does, rather than
  free-text fields, and have the remote script itself validate or quote
  anything it further interpolates internally -- that validation now lives
  entirely outside `subgov`'s reach, so it has to be correct on its own.

## Credentials and timeouts

- **SSH auth is whatever key-based auth already exists on the host**
  (this environment's Tailscale SSH model is the intended fit) --
  `subgov` never manages, stores, generates, or is configured with an SSH
  credential of any kind. There is no `ssh_key_path` or equivalent config
  field; the `ssh` binary's own ambient configuration (agent, known
  `~/.ssh/config`, Tailscale SSH) is what authenticates, exactly as it
  would for a human running the same command by hand.
- **Use a bounded `ConnectTimeout`** in the SSH invocation itself (e.g.
  `argv: [ssh, -o, ConnectTimeout=5, lab.tailnet, resource-probe]`) so a
  host that's unreachable fails fast rather than consuming the whole
  command timeout just establishing the connection.
- **`subgov`'s own command timeout applies to the entire SSH round trip**,
  not just local process startup -- `CHILD_TIMEOUT` in `src/fleet.rs`
  (and the equivalent in `src/source.rs` for a resource/quota `command`
  source) bounds the whole child process, including however long `ssh`
  itself takes to connect, authenticate, run the remote command, and
  return output, and kills the complete process group on expiry (§11.1/
  §11.2, "apply a command timeout and kill the complete child process
  group"). There is no separate, larger budget for the network hop; size
  `timeout_seconds` generously enough to cover a slow SSH connection
  attempt on top of the remote script's own runtime, since both come out
  of the same budget.

## What doesn't change

Everything else about a `command` source/observer/actuator applies
identically whether `argv[0]` is `ssh` or a local binary: output is still
bounded (§7.4's size caps), the child process group is still killed on
timeout, and a failure on this account/host is still isolated from every
other account and host (§7.4 "a source error affects only its account and
cannot actuate its fleet"). SSH is a transport choice for the local
process's own argv, not a new code path.
