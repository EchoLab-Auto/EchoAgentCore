# Shell entrypoints

These scripts are thin, fail-closed adapters around explicit `echo-deploy`
subcommands. They never download, clone, build, install packages, elevate
privileges, invoke a service manager or invoke a container runtime.

| entrypoint | default fixed command | explicit mode | writes |
|---|---|---|---|
| `bootstrap.sh` | verify a local tool by SHA-256, then exec it | `--wrapper NAME` execs a sibling entry with the verified tool | nothing |
| `install.sh` | `install-plan` (render-only) | `--files` → `install-files` (new unit/launcher files + ledger) | a new plan dir, or new files only |
| `update.sh` | `apply` (explicit transaction via admin adapter) | none | transaction state via the CLI |
| `repair.sh` | `recover` (interrupted transaction only) | none | recovery state via the CLI |
| `uninstall.sh` | `uninstall-plan` (render-only) | `--files` → `uninstall-files` (ledger-owned files only) | nothing, or removes ledger-owned files |
| `compose-plan.sh` | `compose-plan` (render-only) | none | nothing |

All wrappers except `bootstrap.sh` require `ECHO_DEPLOY_TOOL` to be an
absolute path to a regular, non-symlink, executable file. There is no `PATH`
fallback. `bootstrap.sh` takes the same file via `--tool`, verifies it against
an explicit SHA-256, and can hand it to any wrapper through `--wrapper`.

## What is deliberately absent

- No config rewrite. `--overwrite-config` is refused by `install.sh`; an
  existing `core.toml`/`panel.toml` is never silently replaced.
- No data purge. `--purge` is refused by `uninstall.sh`; configuration, state,
  media, sessions and user skills are never removed by these scripts.
- No implicit service activation. Installing unit/launcher files is not
  enabling, starting, restarting or reloading a service; only `update.sh`
  (`apply`) and `repair.sh` (`recover`) invoke the explicit administrator-owned
  transaction adapter.
- No build/download self-update. `update.sh` only applies an already-verified,
  already-staged candidate through an explicit administrator-owned adapter.
- No container execution. `compose-plan.sh` renders a plan; a plan is not a
  deployment and does not change running containers.

## Typical commands

```bash
# Verify a local tool once, then run a wrapper with exactly that tool.
scripts/bootstrap.sh --tool /opt/echo-deploy --sha256 "$(sha256sum /opt/echo-deploy | cut -d' ' -f1)" \
    --wrapper install -- --spec /srv/echo/install.json --output /srv/echo/plan-20261115

# Plan only (the default).
ECHO_DEPLOY_TOOL=/opt/echo-deploy scripts/install.sh \
    --spec /srv/echo/install.json --output /srv/echo/plan-20261115
ECHO_DEPLOY_TOOL=/opt/echo-deploy scripts/uninstall.sh --spec /srv/echo/install.json
ECHO_DEPLOY_TOOL=/opt/echo-deploy scripts/compose-plan.sh --spec /srv/echo/compose.json

# File-only install: new files only; collisions are hard errors.
ECHO_DEPLOY_TOOL=/opt/echo-deploy scripts/install.sh --files \
    --spec /srv/echo/install.json \
    --unit-directory /home/me/.config/systemd/user \
    --launcher-directory /home/me/.local/bin \
    --ledger /srv/echo/ledger-20261115.json --confirm

# File-only uninstall: only ledger-owned files; the operator attests offline.
ECHO_DEPLOY_TOOL=/opt/echo-deploy scripts/uninstall.sh --files \
    --ledger /srv/echo/ledger-20261115.json --offline-only --confirm

# Real transaction (requires a staged release and an admin-owned adapter).
ECHO_DEPLOY_TOOL=/opt/echo-deploy scripts/update.sh \
    --execution /srv/echo/execution.json --release 20261115-01 --confirm
```

## Legacy flags

The old repository `scripts/install.sh`, `scripts/update.sh`,
`scripts/uninstall.sh` and `scripts/reinstall.sh` are **not replaced** by these
entrypoints yet. Their build, dependency bootstrap, config rewrite, service
activation and rollback paths have no equivalent here. Legacy flags are
rejected with exit code 2 instead of being forwarded:

| legacy flag | this entrypoint |
|---|---|
| `--overwrite-config` | refused (no config rewrite) |
| `--purge` | refused (no data/config deletion) |
| `--disable-linger` | refused (no service-lifetime change) |
| `--scope`, `--service-user` | refused (scope belongs to the install spec) |
| `--remote`, `--local`, `--source`, `--binary`, `--status`, `--full`, `--modules-only` | refused (no build/download self-update) |
| `--no-deps`, `--no-start`, `--repository`, `--cn-mirror*` | refused (no dependency/bootstrap/start path) |
| `--dry-run` | accepted as plan mode for `install.sh`/`uninstall.sh`; refused with `--files` |

Exit codes: argument/tool/usage errors are `2`; CLI runtime failures are the
CLI's own exit code (`1`); a successful exec preserves the CLI's `0`.
