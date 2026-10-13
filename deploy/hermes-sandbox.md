# Mounting `karl` into the Hermes sandbox

Hermes runs the model's shell commands in a rootless podman container with
`--network=none`. Two bind mounts give that container the `karl` CLI and the
profile socket, and nothing else. Unix sockets are files: they cross the
network namespace.

In the Hermes role, `config.yaml.j2`, block `terminal`:

```yaml
  docker_extra_args:
    - "--cap-drop=ALL"
    - "--security-opt=no-new-privileges"
    - "--pids-limit=256"
    - "-v"
    - "/opt/karl/bin/karl:/usr/local/bin/karl:ro"
    - "-v"
    - "/run/karl/{{ item.name }}.sock:/run/karl.sock"
```

`item.name` is the profile (`alex`, `asami`): each profile mounts its own
socket, so the daemon knows who is asking from the path alone.

Requirements on the host:

- `karl` is the static musl build (`cargo build --release -p karl --target x86_64-unknown-linux-musl`), copied to `/opt/karl/bin/karl`, mode 0755.
- `/run/karl` is 0755 (`RuntimeDirectoryMode=0755` in the unit) and the profile
  sockets are 0666 (`socket_mode = "0666"` in `[daemon]`). A group bit would not
  reach the sandbox: rootless podman with **runc** drops the host user's
  supplementary groups (`--group-add keep-groups` is crun-only). The gate is the
  daemon's peer-uid check, not the file mode. On crun you can keep 0660 +
  `socket_group` + `keep-groups` instead.
- `karld` must be up before a sandbox is created: podman creates a *directory*
  at a missing bind source. `karld` removes an empty directory at its socket
  path on start, and the container is recycled after `lifetime_seconds`.

Inside the sandbox, uid 0 is the host's `hermes` uid (rootless podman), so the
daemon's peer-credential check sees `hermes`, which is the uid listed in the
profile.

Tell the model about it in `SOUL.md`, the same text `karl` prints with no
arguments (keep both in sync; it is ~80 tokens):

```
Personal data via the `karl` command in the terminal (read-only, pre-digested, one line per item):
  karl news         everything new since last asked; karl jira news = one source
  karl msg  news | recent 2h | show <who> [n] | find <words>     (chats)
  karl mail news | recent 2h | show <who> [n] | find <words>
  karl cal  today | tomorrow | week | <yyyy-mm-dd>
  karl jira mine | news | show KEY | comments KEY | q <JQL>
Run `karl` alone for the exact list. Output is already short: do not ask for more than you need.
```
