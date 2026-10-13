# Karl

**A personal data plane for an AI agent.** Karl pulls your mail, calendar and
Jira into one local cache, and hands an agent one-line answers through a
unix socket. The agent never sees a credential, a raw API payload or a
database. It sees text like this:

```text
$ karl news
gm Alice Smith: Re: invoice Q3 — can you check the attached…(+340c)
gcal thu 15 10:00-11:00 Standup
jira HT-36 To Do Alexandre Karlsson: Shorten R2S warm-up (328 s) or report partial results
```

Three goals, in order:

1. **Token efficiency.** The agent runs on a CPU model where prompt tokens are
   seconds. So: zero tool schemas (it is a shell command), one line per item,
   names instead of ids, bodies cut, noisy chats folded, `news` returns only
   what changed since the last ask, and `karl jira news` asks one source.
2. **Security.** Credentials live in a daemon under its own user. The agent's
   sandbox gets a socket and a 500 KB static binary, nothing else. Writes are
   off by default, and when allowed per source they are either executed or
   queued for a human. Every call is policy-checked and audited.
3. **Performance.** Sync runs in the background; a tool call reads SQLite and
   returns in milliseconds. Rust end to end, no runtime, no Python on the box.

## The pieces

| Binary | Runs as | Holds | Does |
| --- | --- | --- | --- |
| `karld` | `karl` | credentials, cache, policy | syncs sources, serves one socket per profile, enforces policy, writes the audit log |
| `karl` | whoever calls it (the agent's sandbox) | nothing | sends argv to the socket, prints the reply, exits with its code |

Today the agent is [Hermes](https://github.com/NousResearch/hermes-agent) on a
mini PC. Karl does not care: any process that can run `karl` is a client, and
a future Rust harness talks to the same socket. See [Own harness](harness.md).

## Status

Working: Jira (sync + live `q` and `comments`, writes `comment` and `move`),
Gmail and Google Calendar sources (written against the official APIs, need an
OAuth client to test), policy with three write levels and an approval queue,
audit, rate limit, hide and redact rules, the static CLI, the systemd unit and
the egress table. Not yet: chat sources (LINE, WhatsApp, Messenger have no
official personal API; Discord would be official), a UI.

## Where to read next

- [Architecture](architecture.md): the two halves and the socket between them.
- [One request](request.md): what happens between "anything new?" and the answer.
- [Security](security.md): who holds what, who can reach what, what Karl does not protect.
- [Commands](commands.md), [Protocol](protocol.md), [Policy file](policy.md): the three contracts.
- [Deploy](deploy.md): hermes1, the sandbox mounts, ansible.
