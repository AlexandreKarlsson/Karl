# Sources

A source turns a provider into `Item`s and lets the store decide what is new.
Each implements one trait:

```rust
#[async_trait]
pub trait Source: Send + Sync {
    fn id(&self) -> &str;                 // "gm", "jira": the policy key, the line prefix
    fn kind(&self) -> &'static str;       // "gmail", "jira", "gcal"
    fn group(&self) -> &'static str;      // the command group it feeds: msg | mail | cal | jira
    fn news(&self) -> News;               // Changes (edits are news) or Arrivals (only new items)
    async fn sync(&self, store: &SharedStore) -> Result<SyncReport>;
    async fn live(&self, what: Live<'_>) -> Result<Option<String>> { Ok(None) }
    async fn write(&self, what: &WriteReq) -> Result<String> { bail!("cannot write") }
    async fn preflight(&self) -> Result<()> { Ok(()) }
}
```

A source says itself which command group it feeds and what "news" means for
it; the daemon learns both from the trait, not from a table. Reads are `GET`.
A write is a `WriteReq` (`Comment { key, text }` or `Move { key, status }`)
that the daemon hands over only after the profile's mode allowed it or a
human approved it; a source with no `write` cannot write.

## Jira (`kind = "jira"`)

| Key | |
| --- | --- |
| `site` | `https://<you>.atlassian.net` |
| `secret` | table with `email` and `token` (an Atlassian API token) |
| `window_days` | issues updated in the last N days, default 30 |
| `jql` | optional extra filter, AND-ed: `project in (HT, GPD)` |

Sync: `updated >= -Nd ORDER BY updated DESC`, paged, with summary, status
(and its category, which sets `done`), priority, assignee, reporter, type,
project, labels and the description converted from Atlassian Document Format
to text. The account id of the token's owner is fetched once and cached, so
`jira mine` is a local query.

Live: `jira q <JQL>`, `jira comments KEY`, and `jira show KEY` for an issue
outside the window.

Writes (when the profile's mode allows): `comment KEY text` posts a comment;
`move KEY status` looks up the issue's transitions and posts the one whose
name matches, case-insensitive, or fails listing the names it found. Both go
through one `post` method that accepts `/rest/api/3/issue/…` paths and
nothing else.

## Gmail (`kind = "gmail"`)

| Key | |
| --- | --- |
| `secret` | table with `client_id`, `client_secret`, `refresh_token` |
| `scopes` | `["https://www.googleapis.com/auth/gmail.readonly"]` |
| `window_days` | messages newer than N days, default 14 |

Sync: list ids newer than the window (spam and trash excluded), fetch the ones
the store does not have, 60 per pass, oldest first. Body: `text/plain` part,
else `text/html` stripped; quoted replies and signatures removed; cut at 4000
chars. Labels are resolved to names so `hide = ["gm:label=Bank"]` works.

Known limit: a mail already stored is not re-fetched, so later label changes
(read, archived) are not reflected. On purpose: otherwise every read mail
would be "new" again.

## Google Calendar (`kind = "gcal"`)

| Key | |
| --- | --- |
| `secret` | same Google table as Gmail; one consent covers both |
| `scopes` | `["https://www.googleapis.com/auth/calendar.readonly"]` |
| `calendar` | `primary` or a calendar id |
| `days_ahead` | window end, default 14 |

Sync replaces the window `[now − 1 day, now + days_ahead]`: recurring events
expanded, cancelled ones skipped, events gone upstream deleted. Visibility
becomes a label, location a `at:` label, the description is stripped and
cut at 1000 chars.

## Google OAuth, once

1. In Google Cloud Console create an OAuth client of type **Desktop app**;
   enable the Gmail and Calendar APIs; put `client_id` and `client_secret`
   in the secrets table.
2. On the box, as the daemon user, with the browser on your laptop:

```sh
ssh -L 8765:127.0.0.1:8765 hermes1
sudo -u karl /opt/karl/bin/karld auth google-alex --port 8765
```

   It prints a URL; open it on the laptop; the consent screen redirects to
   `127.0.0.1:8765`, which the ssh tunnel carries back to the box. The refresh
   token is written into the secrets file.
3. `karld check` then confirms the scopes are exactly the read-only ones.

A Google "desktop" client only accepts loopback redirects, which is why the
tunnel is needed rather than a LAN URL.

## Not here, and why

| Wanted | Status |
| --- | --- |
| LINE | no API for a personal account. The Messaging API is for bots. Candidates: an Android notification relay (official API, every app, text only), or an unofficial client with ban risk |
| WhatsApp | no official personal API. Unofficial Rust clients exist (`wa-rs`); Meta may ban the account |
| Messenger | no official personal API; browser automation only |
| Matrix | one source would cover WhatsApp, Messenger, Signal, Telegram through mautrix bridges (Go). Infra to run, same ban risk for the bridged networks |
| Yahoo calendar | which Yahoo? Yahoo US speaks CalDAV; Yahoo! JAPAN needs checking |
| IMAP | straightforward; Gmail covers the first need |

## Adding a source

One file in `karl-sources`, one line in `build`. Say a Discord reader:

1. `crates/karl-sources/src/discord.rs`: a struct built from its
   `[source.<id>]` table (`extra_str`, `extra_u64`) and the secrets table.
2. `impl Source`: `group()` returns `"msg"`, `news()` returns `News::Arrivals`,
   `sync` lists the channels it may read and upserts one `Item` per message
   (`Kind::Msg`, `thread` = channel id, `thread_name` = channel name, `who` =
   author, `body` = text). `preflight` calls `/users/@me`. No `write`.
3. `karl_sources::build`: `"discord" => Box::new(discord::Discord::new(id, cfg, secrets)?)`.
4. Policy: `[source.dc] kind = "discord" secret = "discord-bot"`, add `dc` to
   a profile's `sources`. `karld check` must pass.

That is all: the `msg` commands, the cursor, folding of busy channels, hide
and redact rules, the index line and the audit log come from the daemon.
A write-capable source adds `write` and the daemon's three modes apply to it
unchanged.

## CalDAV and `.ics` (`kind = "caldav"`)

| Key | |
| --- | --- |
| `url` | the calendar collection, `https://…/Calendar/<id>/` (CalDAV), or any `https://….ics` feed |
| `secret` | table with `user` and `password`; optional for a public feed |
| `days_ahead` | window end, default 14; the window starts one day ago |
| `feed` | `true` to force GET mode on a url that does not end in `.ics` |

Sync: one `REPORT calendar-query` with a `time-range` and `expand`, so the
server expands recurrences and every instance is one item (`UID@RECURRENCE-ID`).
The window is replaced on every pass: an event gone upstream is deleted.
`TZID` times are read in the daemon's local zone (`TZ` in the unit); `Z` is
UTC; `VALUE=DATE` is an all-day event. A plain `.ics` feed is parsed the same
way but without expansion: a recurring event shows its first instance only.

**Yahoo! JAPAN カレンダー**: カレンダー設定 → 他のアプリと同期 (CalDAV); use
an app password, not the account password. The collection url is
`https://caldav.yahoo.co.jp/dav/<Yahoo! JAPAN ID>/Calendar/<calendar-id>/`;
the calendar id is shown in the same settings page. To list the collections a
server exposes:

```sh
curl -u "$USER:$PASS" -X PROPFIND -H 'Depth: 1' "https://caldav.yahoo.co.jp/dav/$USER/Calendar/"
```

Works the same against Nextcloud, Fastmail, iCloud (app password) and
Radicale. Read-only: the source has no write path.

## Microsoft Teams chats (`kind = "teams"`)

| Key | |
| --- | --- |
| `secret` | table with `client_id`, `tenant` (default `organizations`), `refresh_token` |
| `scopes` | default `offline_access User.Read Chat.Read` |
| `window_days` | messages newer than N days, default 14 |

Needs an app registration (Entra ID → App registrations → *Allow public
client flows: yes*, redirect not needed) in the tenant, or a personal
registration with "accounts in any organizational directory" if the work
tenant lets users consent to third-party apps. The refresh token comes from
the device-code flow, which works over ssh:

```sh
scripts/karl dev-auth-ms teams-alex        # prints a url + code; sign in; writes refresh_token
```

Sync: `/me/chats?$expand=members` ordered by last activity, the 30 most
recent chats touched inside the window, 30 messages each. Items are chat
messages: `who` is the sender (or `me`), `thread_name` the chat topic or the
other members, body from HTML. System messages (members added, calls) are
skipped. **Channel messages are not read**: `ChannelMessage.Read.All` needs
admin consent, which "restricted" tenants do not give, and the source does
not ask for it. Microsoft rotates the refresh token on every use; the daemon
keeps the newest in memory and writes it back to the secrets file.

## Matrix (`kind = "matrix"`) — WhatsApp, Messenger, Instagram

| Key | |
| --- | --- |
| `homeserver` | `https://matrix.example.net` |
| `secret` | table with `access_token` and `user_id` (`@you:matrix.example.net`) |
| `protocol` | keep only rooms whose `m.bridge` state says this: `whatsapp`, `facebook`, `instagram`. Unset = every joined room |
| `window_days` | ignore events older than N days, default 14 |

Meta has no personal API. A [mautrix](https://github.com/mautrix) bridge on
your own homeserver (`mautrix-whatsapp`: WhatsApp multi-device, stable;
`mautrix-meta`: Messenger and Instagram, against Meta's terms, bans happen)
turns each chat into a Matrix room; Karl reads the rooms with the
client-server API and knows nothing about Meta. One source per network, so
the line prefix tells the model where a message came from (`wa`, `fb`, `ig`).

Sync: `/sync` with the stored `next_batch` (first pass: 30 events per room,
lazy-loaded members), `m.room.message` events only; images, files and voice
become `[image]`, `[file name]`, `[voice]`; a reply keeps only the new text.
The bridge protocol and the room name are state, learned once and remembered
in the store meta. Read-only.

