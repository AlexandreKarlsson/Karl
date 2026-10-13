# Commands

What the model types. One table in `karl-core::commands` drives the parser,
the index and the dispatcher, so the three cannot drift.

## The index

`karl` with no arguments prints this, and the same text sits in the agent's
system prompt so no round trip is needed. Groups without a source for the
profile are left out.

```text
karl news = everything new. `karl` alone = this index.
  mail news | recent <2h|3d> | show <who> [n] | find <words>
  cal  today | tomorrow | week | <yyyy-mm-dd> [<yyyy-mm-dd>]
  jira mine | news | show <KEY> | comments <KEY> | q <JQL> | comment <KEY> <text> | move <KEY> <status>
sources: gm 2m · gcal 7m · jira 1m
```

The `sources:` line shows the age of each source's last successful sync, or
`ERR`. A group's write verbs (`comment`, `move`) appear only when the profile
is authorized to write there; a `not-authorized` profile never sees them.

## Reference

| Command | Answer |
| --- | --- |
| `karl` | the index above |
| `karl news` | everything new since the last ask, every source, oldest first; advances the cursors; `(more: run again)` when capped. `new` is accepted too |
| `karl jira news` / `mail news` / `msg news` | same, one group: "anything new on Jira?" |
| `karl pending` | this profile's queued writes and their status (`pending`, `done`, `denied`, `failed`) |
| `karl mail recent 2h` | mails of the last 2 hours, newest first, with time. Not consuming |
| `karl mail show alice` | last 5 mails whose sender, address or thread contains `alice` |
| `karl mail show alice 1` | the last one, in full (cut at `max_bytes`) |
| `karl mail find invoice q3` | full-text search, every word a prefix, AND-ed |
| `karl cal today` / `tomorrow` / `week` | one line per day: `today: 10:00-11:00 Standup · 13:00-14:00 Lunch` |
| `karl cal 2026-10-20 2026-10-22` | a range, inclusive |
| `karl jira mine` | open issues assigned to you, most recently updated first |
| `karl jira show HT-36` | the issue: header line, link, type, then the description. Falls back to a live lookup when it is outside the sync window |
| `karl jira comments HT-36` | last 10 comments, live |
| `karl jira q project = HT AND status = "In Progress"` | live JQL, rendered like `mine` |
| `karl jira comment HT-36 text` | a write: refused (exit 3), queued (`queued #12 for approval`), or executed, by the profile's mode on that source |
| `karl jira move HT-36 Done` | a write: transition the issue to the status named (case-insensitive); same three outcomes |

`msg …` commands exist for chat sources; none ships yet, so they answer
`no msg source for this profile`.

## What "news" means, per source

Each source declares what counts as news, and the policy file may override it
(`news = "changes"` or `"arrivals"` in the source table):

| Strategy | Means | Default for |
| --- | --- | --- |
| `changes` | anything inserted *or edited* since the cursor: a task whose status moved, an event rescheduled | Jira, Calendar |
| `arrivals` | only items that *arrived* since the cursor; later edits are not news | Gmail, chat |

The cursor is per profile *and per source*, so `karl jira news` and
`karl mail news` never disturb each other, and `karl news` is simply all of
them. The first ask on a source sets its cursor to now.

## Writes

Three levels, per profile and per source, in the policy file:

| `mode` | `karl jira comment …` answers |
| --- | --- |
| `not-authorized` (default) | exit 3, `writes not authorized for jira`. The verb is not even in the index |
| `authorization-required` | exit 0, `queued #12 for approval: comment HT-36: …`. A human runs `karl admin approvals`, then `approve 12` or `deny 12`. `karl pending` shows the outcome |
| `authorized` | executes now, answers the provider's confirmation (`commented on HT-36 (id 10423)`) |

Every executed, queued, approved or denied write is one line in the audit log.

## Line formats

Every line starts with the source id, so the kind is implied and the model
can address the source back.

| Kind | Line |
| --- | --- |
| mail | `gm Alice Smith: Re: invoice Q3 — can you check the attached…(+340c)` |
| chat | `line Juliette: Hey what are you doing tonight brother?` · folded: `wa Family(+14) Mom: dinner sunday?` |
| event | `gcal 10:00-11:00 Standup` · not today: `gcal thu 15 all-day Holiday` |
| task | `jira HT-36 To Do Alexandre Karlsson: Shorten R2S warm-up` (priority shown only when not Medium) |

Rules that save tokens: newlines become ` ⏎ `, bodies are cut to the line
budget with `…(+Nc)`, HTML is stripped at sync time, quoted replies and
signatures are dropped, chat threads with more than three new messages fold
into one line, dates are omitted for today, and `(+N more)` replaces the tail
of a capped list.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | ok, body on stdout |
| 1 | daemon error (body on stderr) |
| 2 | usage: unknown command, bad argument, no source for that group |
| 3 | denied by policy (a write) |
| 4 | rate limited, body says when to retry |
