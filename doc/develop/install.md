# Installation and controlled self-update

## Scope

The installer targets Linux hosts with a systemd user manager. It does not
require root and does not modify system-wide directories.

```bash
./scripts/install.sh [--owner-qq QQ_ID] [--no-start]
```

Use `./scripts/install.sh --dry-run` to inspect all resolved paths without
changing the machine. `ECHO_PREFIX`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, and
`XDG_STATE_HOME` are respected.

The Panel (TUI) is **not** part of this repository — install it separately
from EchoAgentPanel's own `scripts/install.sh`.

## Installed layout

| Path | Purpose |
|---|---|
| `~/.local/bin/echo-agent-core` | Core launcher |
| `~/.local/libexec/echo-agent-core/` | Runtime binary and updater |
| `~/.config/echo-agent-core/core.toml` | Persistent Core configuration |
| `~/.local/share/echo-agent-core/source` | Clean managed Git checkout |
| `~/.local/state/echo-agent-core/` | Update lock, revision and status |
| `~/.config/systemd/user/echo-agent-core.service` | Long-running Core |
| `~/.config/systemd/user/echo-agent-core-update.service` | One-shot updater |

Existing configuration files are preserved. Re-running the installer refreshes
the binary, updater and service units. The installer enables self-update in an
existing config without changing API credentials or other settings.

Re-running the installer is transactional after the release build completes:
the current binary, launcher, updater, configuration, units and revision state
are snapshotted under `~/.local/state/echo-agent-core/install-rollback/`.
That snapshot is removed only after `daemon-reload` and the Core active-state
health check succeed. If activation or a later install step fails, all changed
files are restored and the previous Core is restarted when it was running.

The Core user service is enabled under `default.target`. Whether it starts at
machine boot before an interactive login depends on the host's user lingering
policy. An administrator can enable that explicitly:

```bash
sudo loginctl enable-linger "$USER"
```

## Update flow

```text
authorized conversation
        |
        v
framework_update tool (fixed actions only)
        |
        v
systemctl --user start echo-agent-core-update.service
        |
        +--> flock --> git fetch --> fast-forward check
        |                         |
        |                         v
        +-------------------- cargo build --locked
                                  |
                                  v
                         atomic binary replacement
                                  |
                                  v
                       Core restart + health check
                            |             |
                          healthy       failed
                            |             |
                          finish       rollback
```

The Agent cannot provide a command, repository, path, branch, or service name to
the tool. The only accepted actions are `status` and `apply`; `apply` also
requires `confirm=true`. The service name is compiled as
`echo-agent-core-update.service`.

Authorization is evaluated from the real session ID:

- local TUI sessions are allowed when `allow_local = true`;
- QQ sessions must match `[adapters.qq].owner_qq` or an entry in
  `allowed_qq_users`;
- all other platforms and malformed session IDs are denied.

```toml
[agent.self_update]
enabled = true
allow_local = true
allowed_qq_users = [123456789]
```

## Failure behavior

The updater takes a non-blocking file lock and refuses concurrent runs. It also
refuses to update when the managed checkout has tracked local changes, is on a
detached HEAD, or the remote update is not a fast-forward. Untracked custom
skills are left in place.

Git advances before compilation, but the installed binary is replaced only
after a successful locked release build. A fetch or build failure therefore
leaves the old Core binary installed and does not restart it. The next update
attempt rebuilds the managed revision because the installed revision is tracked
separately. When Core was running, the updater also retains the previous binary
until the restarted service passes a 15-second active-state health check. A
failed check, updater refresh failure, or commit/state write failure restores
the old binary and updater and restarts it, recording `state=rolled_back`.

Inspect an update with:

```bash
systemctl --user status echo-agent-core-update.service
journalctl --user -u echo-agent-core-update.service
cat ~/.local/state/echo-agent-core/update-status
```

Manual triggering uses the same constrained path as the Agent:

```bash
systemctl --user start echo-agent-core-update.service
```

## Install the current local worktree

For development builds, use the updater's explicit local mode. It skips Git
fetch and merge, then compiles the current worktree including uncommitted
changes:

```bash
scripts/update.sh --local \
  --source "$PWD" \
  --binary "$HOME/.local/libexec/echo-agent-core/echo-agent-core-bin" \
  --status "$HOME/.local/state/echo-agent-core/update-status"
```

Local mode always rebuilds. It records `local:<commit>+dirty` when the worktree
has changes. The systemd updater remains remote-only and continues to require a
clean managed checkout.
