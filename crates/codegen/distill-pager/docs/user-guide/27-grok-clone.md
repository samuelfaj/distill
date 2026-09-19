<!-- Modified for Distill by Samuel Fajreldines, 2026. -->
# grok clone

`distill clone` fetches a Git repository into a Grove content store and mounts a
projected working tree (NFS on macOS, FUSE on Linux). Each invocation reads
`GROK_CLONE` / `GROVE_CLONE` in this process, then grok enable-all
(`GROK_GROVE` or `[cli] grove` in `~/.grok/config.toml`), then `[clone] enabled`
authorize Clone IPC.

This does **not** enable Grove for session / `-w` worktrees. Those use a
separate gate (`GROK_WORKTREE_TYPE` and `[cli] grove_worktree` in
`~/.grok/config.toml`; see [Configuration reference](26-config-reference.md)).
`GROK_WORKTREE_TYPE` and `[cli] grove_worktree` do **not** enable `distill clone`.
`GROK_CLONE` / `GROVE_CLONE` / `[clone] enabled` do **not** enable session /
`-w` Grove.

To turn **both** surfaces on without touching the specific knobs:

```bash
export GROK_GROVE=1
# or in ~/.grok/config.toml:
# [cli]
# grove = true
```

Specific knobs still win: `GROK_WORKTREE_TYPE=copy` keeps session worktrees on
copy while clone can stay on; `GROK_CLONE=0` keeps `distill clone` off while
worktrees can stay on.

```bash
distill clone <url> [dir] [--branch NAME] [--cone PATH]... [--full-history]
```

## History

**Default is a depth-1 bootstrap** of the selected branch (`blob:none` +
`--depth=1`). Only that branch is advertised as a remote-tracking ref.

Use `--full-history` when you need complete commit history, tags, or every
remote branch at clone time (the previous default).

After a depth-1 clone, these commands deepen **only the selected branch**:

```bash
git fetch --deepen=N origin
git fetch --unshallow origin
```

Fetching another branch needs an explicit depth-limited refspec. An ordinary
`git fetch origin` or `git fetch origin other` will not pull that branch's
full history through the default refspec:

```bash
git fetch --depth=1 origin refs/heads/NAME:refs/remotes/origin/NAME
```

`clone_shallow` RPC. If the client refuses, restart or update the daemon (or
pass `--full-history`):

```bash
```

On macOS you can also install a KeepAlive agent:

```bash
```

## Authentication

The two are separate worlds:

| World | Covers | Commands | Store |
|-------|--------|----------|-------|
| Grok | the model and API | `distill login`, `distill logout` | `~/.grok/auth.json` |

`distill clone` never reads `~/.grok/auth.json` for Git. Signing into Grok does not
give the daemon a credential for the remote, and neither does
`[clone] enabled = true`: that flag is a **product gate** deciding whether
`distill clone` runs at all, not authorization for GitHub.

When Grove classifies a failure as a credential problem, the clone prints the
class and the commands that own it, without the remote URL:

```
Grove Git credentials rejected (unavailable).
Check `grove status` then `grove reload-credentials`.
```

| Class | Meaning | Next step |
|-------|---------|-----------|
| `unavailable` | the daemon had no usable credential, or the remote rejected it | `grove status` names the live provider; fix that source, then `grove reload-credentials` |
| `expired-static` | the token expired and this deployment does not refresh tokens | start a new session, or enable `GROVE_TOKEN_ROTATION=expected` |
| `carrier-stale` | the daemon waited for a carrier rewrite and still holds the rejected token | wait for the rewrite, or `grove reload-credentials` |
| `other` | the credential provider failed for another reason | `grove status`, then `grove reload-credentials` |

Only `expired-static` and `carrier-stale` add their own line to the message. The
advice for `unavailable` depends on which provider the daemon holds, and the

One credential rejection stays unclassified: when a token cannot see a private
repository, GitHub answers as if the repository did not exist. That is
indistinguishable from a typo in a public URL, so the clone reports it as a
missing repository rather than guessing at credentials.

`grove status` prints a daemon-scoped auth block even with no mounts:

```
  auth: mode=auto live=git-delegate health=ok last=- reload=supported
        hint=credentials look healthy
```

`mode` is the configured `auth_mode`; `live` is the provider the daemon actually
holds. `mode` and `live` can disagree — that is the case
`grove reload-credentials` exists to fix. For example, with `auth_mode = auto`,
a daemon that started before a token file was written stays on `git-delegate`
until the cell is rebuilt.

```bash
grove reload-credentials
# grove: credentials reloaded → token-file
```

Reload rebuilds the cell from env and disk and prints the provider it landed on;
it prints no secrets. It cannot create a login: with `auto` or `git` and no
carrier token, configure `git credential` or `gh auth` first, then reload.

`auth.degraded`, `auth.unavailable`, `auth.daemon-down`, or `auth.old-daemon`.

## Daemon

`grok` does not take the daemon or its mounts down) and waits for the socket.

The `grove` binary is resolved from `PATH`, then from the directory of the
`grok` executable (for example `~/.grok/bin/grove` next to `grok`). There is
no separate install location. macOS has no PATH package for grove; build it
from the monorepo:

```bash
cargo build -p grove --release
```

On Linux, clone needs a usable FUSE before it starts a daemon: `/dev/fuse` must
exist, and either this user can open it or a setuid `fusermount3` / `fusermount`
error with install commands, not a hang. When a daemon is already running the
check is skipped, since that daemon may hold privileges this process does not.

Windows is not supported (no ProjFS backend). Use `git clone`, or run
`distill clone` on macOS or Linux.
