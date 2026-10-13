# How Karl works

// A tour of Karl: a personal data plane for an AI agent. Four crates, one Item, one
// socket, and a policy the model never sees. Project terms: glossary.md.
plan: Each part starts with the simple idea, a picture, then the code: a robot in a locked room, a librarian with the keys, cards, a ticket counter, a window, a checklist, bookmarks, a tray for writes, one line per card, two rulebooks, a messenger, the librarian's day, and what comes next.
pronounce: karld = karl dee
pronounce: Karl = karl
pronounce: SQLite = sequel light
pronounce: rusqlite = rus sequel light
pronounce: argv = arg vee
pronounce: tokio = toe kee oh
pronounce: podman = pod man
pronounce: musl = mussel
pronounce: nftables = n f tables
pronounce: Hermes = hermees
pronounce: Jira = jeera
pronounce: seq = seek
pronounce: ext_id = external id
pronounce: upsert = up sert
spell: ID, IP, UID, OTP, JQL, JSON, CLI, API, TOML, TTS, CPU, LLM, LAN, MCP, HTTP, HTML, URL, SQL, WAL, FTS5, SHA, GET, POST, AI, uid, ids, id

- [Welcome](01-welcome.md) — a robot in a locked room, a librarian with the keys, and the four crates
- [One record for everything](02-one-item.md) — one kind of card for mail, chats, events and issues, with a fingerprint
- [The cache and the seq counter](03-the-store.md) — a drawer and a bakery ticket counter: how "new" becomes one number
- [The client](04-the-client.md) — the window in the robot's room: no keys, no memory, no brain
- [Through the socket](05-the-socket.md) — the librarian's side of the window: badge, one question, one answer, the logbook
- [Enforcement in order](06-the-dispatcher.md) — the checklist before any lookup: on the menu, allowed to write, have the shelf, not too often
- [What news means](07-new.md) — a bookmark per messenger, two ideas of "new", first ask starts today, "more" means run again
- [Write permissions](08-write-permissions.md) — no, ask me, or yes: the tray on your desk, and what the robot and you each see
- [One line per item](09-render.md) — one short line per card, noisy chats folded, nothing that is not information
- [Two contracts](10-contracts.md) — the menu the robot reads, the house rules it never sees, and the three write permissions
- [A source: Jira](11-jira.md) — a messenger: every five minutes, thirty days back, oldest first, and two small things it may write
- [The daemon boots](12-daemon.md) — the librarian's day: check the key safe, open the drawer, send messengers, open windows, wait
- [Where this goes](13-where-this-goes.md) — Hermes today, a Rust robot tomorrow, the same window either way
