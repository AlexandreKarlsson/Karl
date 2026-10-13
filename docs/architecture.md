# Architecture

Two halves on one host, one unix socket between them.

```mermaid
flowchart LR
  subgraph hermes["uid hermes (exists today)"]
    gw["Hermes gateway<br/>Discord ⇄ ai1"]
    sb["podman sandbox<br/>--network=none"]
    cli["karl (static CLI)"]
    gw -->|terminal tool| sb
    sb --> cli
  end
  subgraph karl["uid karl (new)"]
    d["karld"]
    db[("SQLite cache")]
    sec["secrets 0600"]
    pol["karl.toml policy"]
    aud["audit.log"]
    d --- db
    d --- sec
    d --- pol
    d --- aud
  end
  cli ==>|"/run/karl/alex.sock"| d
  d -->|"tcp/443, 993"| apis["Gmail · Calendar · Jira"]
```

## Crates

```text
crates/
  karl-core      lib  Item model, SQLite store, policy, renderer, command table, wire protocol
  karl-sources   lib  jira, gmail, gcal: each implements Source and syncs into the store
  karld          bin  config, sync loops, profile sockets, dispatcher, admin socket, audit
  karl           bin  the client: argv → socket → stdout, ~150 lines, no dependencies worth naming
```

`karl-core` has no network and no async: it is the part a future agent links
directly. `karl-sources` is the only crate that talks to the internet.
`karld` is glue. `karl` knows nothing but the socket path.

## Data flow

```mermaid
flowchart TB
  subgraph sync["background, every N minutes"]
    j["Jira API"] --> S1["jira::sync"]
    g["Gmail API"] --> S2["gmail::sync"]
    c["Calendar API"] --> S3["gcal::sync"]
    S1 & S2 & S3 -->|"Item"| up["store.upsert_many"]
    up -->|"content changed?"| seq["seq += 1"]
  end
  subgraph call["at call time, milliseconds"]
    req["karl jira news"] --> cur["cursor per profile per source"]
    cur --> q["since_seq(strategy) / recent / find / events_between"]
    q --> hide["hide rules"]
    hide --> render["render: one line per item"]
    render --> redact["redact"]
    redact --> cap["cap: max_lines, max_bytes"]
  end
  seq -.-> q
```

One `Item` type for everything. A mail, a chat message, an event and an issue
differ by `Kind` and by which optional fields are filled; they share the
table, the cursor, the full-text index and the renderer.

`seq` is a global counter bumped on every insert and every content change
(hash of the fields a person would notice); `created_seq` is the `seq` an
item was inserted with. "New" is one integer comparison against the
profile's cursor for that source: `seq > cursor` for sources whose edits
are news (Jira, Calendar), `created_seq > cursor` for sources where only
arrivals are (mail, chat). A re-sync that changes nothing creates no news.

## The three contracts

- [Commands](commands.md): what the model types. One table in `karl-core::commands`
  drives parsing, the index and the dispatcher.
- [Protocol](protocol.md): what crosses the socket. Two JSON lines and a body.
- [Policy file](policy.md): what the daemon enforces. Never in the model's context.
