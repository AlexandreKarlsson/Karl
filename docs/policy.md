# Policy file

`/etc/karl/karl.toml`, mode 0640 `root:karl`. The daemon reads it at start.
Nothing in it ever reaches the model: policy costs zero tokens.

```toml
[daemon]
db        = "/var/lib/karl/karl.db"
audit     = "/var/log/karl/audit.log"
secrets   = "/var/lib/karl/secrets.toml"
retention_days = 90
socket_group = "karl-cli"
admin_socket = "/run/karl/admin.sock"

# --- sources: what to sync, with which credential ---
[source.jira]
kind   = "jira"
site   = "https://example.atlassian.net"
secret = "atl-alex"            # table name in secrets.toml
every  = "5m"
window_days = 30               # issues updated in the last N days
news   = "changes"             # what counts as news here (default: the source decides)

[source.gm]
kind    = "gmail"
account = "me@example.com"
secret  = "google-alex"
every   = "3m"
scopes  = ["https://www.googleapis.com/auth/gmail.readonly"]   # token must not exceed these
window_days = 14

[source.gcal]
kind     = "gcal"
secret   = "google-alex"
every    = "10m"
scopes   = ["https://www.googleapis.com/auth/calendar.readonly"]
calendar = "primary"
days_ahead = 14

# --- profiles: who may see what, how much, how fast ---
[profile.alex]
socket    = "/run/karl/alex.sock"
uids      = ["hermes"]
sources   = ["gm", "gcal", "jira"]
# writes, per source: not-authorized (default) | authorization-required | authorized
mode      = { jira = "authorization-required" }
max_lines = 40
max_bytes = 4096
rate      = "60/h"
hide      = ["gm:label=Bank", "gm:from=*@otp.*", "gcal:visibility=private"]
redact    = ["otp"]

[profile.asami]
socket    = "/run/karl/asami.sock"
uids      = ["hermes"]
sources   = ["gcal-shared"]
max_lines = 20
```

`karld example-config` prints a copy to start from.

## `[daemon]`

| Key | Meaning |
| --- | --- |
| `db` | SQLite file. WAL mode. Created on first start |
| `audit` | one line per request |
| `secrets` | TOML with one `[table]` per credential. Must be 0600 or the daemon refuses to start |
| `retention_days` | items older than this are purged hourly, future events excepted |
| `socket_group` | group of the profile sockets (0660). The daemon user must be a member |
| `admin_socket` | 0600, daemon user only |

## `[source.<id>]`

`<id>` is what the model sees at the start of every line (`gm`, `jira`), so
keep it short. `kind` picks the implementation; the other keys belong to it.

| Key | All sources |
| --- | --- |
| `kind` | `jira`, `gmail`, `gcal` |
| `secret` | table in the secrets file |
| `every` | sync interval: `30s`, `5m`, `2h` |
| `scopes` | the OAuth scopes the token may carry. At start the daemon asks Google which scopes the token has and refuses a broader one, so a token minted with write access cannot be used by mistake. Empty = no check |
| `news` | `changes` (inserted or edited since the cursor) or `arrivals` (inserted only). Default: the source's own choice, Jira and Calendar `changes`, Gmail `arrivals` |

Per kind: [Sources](sources.md).

## `[profile.<name>]`

One agent identity. The socket path binds it.

| Key | Meaning | Default |
| --- | --- | --- |
| `socket` | the unix socket this profile answers on | required |
| `uids` | unix users allowed to connect (names or numbers). Root is refused | required |
| `sources` | source ids visible to this profile | required |
| `mode` | per source, write authorization: `not-authorized` (writes refused, exit 3), `authorization-required` (writes queued for `karl admin approve`), `authorized` (writes execute). Short aliases `no`, `ask`, `yes` | `not-authorized` |
| `max_lines` | lines per reply | 40 |
| `max_bytes` | bytes per reply | 4096 |
| `rate` | replies per window: `60/h`, `10/m`, `500/d` | `60/h` |
| `hide` | rows never rendered for this profile, see below | none |
| `redact` | scrubbers applied to the final text: `otp` (6–8 digit codes), `card` (13–19 digit numbers) | none |
| `line_chars` | chars per rendered line before the body is cut | 160 |

### Hide rules

`source:field=glob`. `source` may be `*`. `*` in the glob matches anything;
matching is whole-string and case-insensitive.

| Field | Matches against |
| --- | --- |
| `label` or `visibility` | any label: Gmail label names, Jira labels and `type:Task`, calendar visibility (`default`, `private`, `public`, `confidential`) |
| `from` | sender name or address, task assignee |
| `title` | subject, summary, event title |
| `thread` | thread name or id: a chat group, a Jira project key |

Hidden items are still synced and still advance the cursor. They are never
rendered for that profile.

## Enforcement

The order in `karld::dispatch`: parse → write authorization → source present →
rate → query (or execute / queue the write) → hide → render → redact → cap →
audit. See [One request](request.md).

## Validation

`karld --config /etc/karl/karl.toml check` loads the file, the secrets, builds
every source, calls each provider once (`/myself`, `/profile`, the calendar),
checks token scopes, and prints the profiles. Run it after every change.
