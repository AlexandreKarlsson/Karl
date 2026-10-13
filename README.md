# Karl

**A personal data plane for an AI agent.** Rust. Read-only. Token-minimal.

Karl syncs your mail, calendars, Jira and chats into one local SQLite cache
and answers an agent through a unix socket, one line per item:

```text
$ karl news
gm Alice Smith: Re: invoice Q3 — can you check the attached…(+340c)
gcal thu 15 10:00-11:00 Standup
jira HT-36 To Do Alexandre Karlsson: Shorten R2S warm-up (328 s) or report partial results
```

The agent never holds a credential and never sees a raw API payload. Writes
are off by default; per source they can be queued for your approval or
allowed. Policy, limits and redaction live in the daemon and cost zero prompt
tokens. Built for a [Hermes](https://github.com/NousResearch/hermes-agent)
bot on a CPU model, where every token is time; designed so a future Rust
harness is just another socket client.

| | |
| --- | --- |
| `karld` | daemon, user `karl`: holds credentials, syncs sources, serves one socket per profile, enforces policy, audits |
| `karl` | 500 KB static CLI mounted into the agent's sandbox: argv → socket → stdout |

## Try it

`scripts/karl` is the launcher; no argument prints the menu.

```sh
scripts/karl build          # karld (glibc) + karl (static musl, 500 KB)
scripts/karl test           # cargo test + clippy -D warnings + fmt --check
scripts/karl dev-init       # dev/karl.toml + dev/secrets.toml (0600), gitignored
#   fill dev/secrets.toml:  [atl-alex] email = "…" token = "…"
scripts/karl dev-check      # credentials, scopes, profiles
scripts/karl dev-sync       # one pass
scripts/karl dev-run        # serve in the foreground
scripts/karl cli            # the index the model reads
scripts/karl cli jira mine
scripts/karl admin status
```

## Docs

`docs/` is a [zensical](https://zensical.org) site with mermaid diagrams:
architecture, one request end to end, security model, the three contracts
(commands, protocol, policy file), sources, deploy, the harness plan.

```sh
scripts/karl docs           # zensical serve → http://127.0.0.1:8000
scripts/karl docs-build     # → site/
```

`codecast/` is a spoken tour of the code for
[IwannatRust](../IwannatRust) (`IWR_DIR` if it is not next to this repo):

```sh
scripts/karl codecast          # play it in the browser
scripts/karl codecast-check    # every ref must resolve
scripts/karl brief             # the brief + writing guide, to rewrite parts
```

## Layout

```text
crates/karl-core      Item, Store (SQLite + FTS5), policy, renderer, command table, protocol
crates/karl-sources   jira, gmail, gcal, caldav (Yahoo! JAPAN, any CalDAV, .ics), teams (Graph), matrix (mautrix bridges)
crates/karld          the daemon
crates/karl           the client
deploy/               systemd unit, nftables egress, ansible role skeleton, Hermes sandbox notes
docs/                 the site
codecast/             the tour
scripts/karl          launcher: build, test, docs, codecast, dev daemon
```

## Status

Jira works against a real site (sync, `mine`, `news`, `show`, live `q` and
`comments`; writes `comment` and `move` with the three authorization levels
and the approval queue). Gmail, Google Calendar, CalDAV (Yahoo! JAPAN
カレンダー), Teams chats (Microsoft Graph, `Chat.Read`) and Matrix are written
against the official APIs and need credentials to exercise. WhatsApp,
Messenger and Instagram have no personal API: they arrive through a mautrix
bridge on your own Matrix homeserver, and the `matrix` source reads the
bridged rooms (one source per network, `protocol = "whatsapp"`). LINE has no
route at all. Deployed by the `karl` role of the setup repo. No UI.

MIT.
