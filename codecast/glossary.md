## profile
aliases: profiles
ask: What is a profile?
One agent identity: a socket path, the unix users allowed to connect, the sources it may see, and its limits. Alex and Asami are two profiles on one daemon. The daemon knows which one is asking from the socket path alone.

## source
aliases: sources
ask: What is a source in Karl?
One provider to sync: a Jira site, a Gmail account, a Google calendar. Each has a short id like `gm` or `jira` that starts every rendered line, so the model can address it back.

## cursor
aliases: cursors
ask: What is the cursor?
Per profile and per kind, the last `seq` the profile has seen. `karl new` returns items above it and then moves it forward. The first ask ever sets it to now, so a backlog is not news.

## seq
ask: What is seq?
A global counter in the store, bumped on every insert and on every content change. "New" is `seq > cursor`: one integer comparison, no timestamps, and a re-sync that changes nothing creates no news.

## sandbox
ask: What is the sandbox?
The rootless podman container where Hermes runs the model's shell commands, with no network. Karl adds two bind mounts to it: the static `karl` binary and the profile's socket. Nothing else.

## hide rule
aliases: hide rules, hide
ask: What is a hide rule?
`source:field=glob` in the profile: items matching it are never rendered for that profile. Bank mail, OTP senders, private calendar events. They are still synced and still move the cursor.

## mode
aliases: modes, authorization, permission
ask: What is a mode?
A profile's write permission on one source: not-authorized, which refuses every write; authorization-required, which queues the write for a human to approve or deny; and authorized, which executes it at once. Reads are always allowed. The default is not-authorized.

## strategy
aliases: changes, arrivals
ask: What are changes and arrivals?
The two meanings of "new". Changes: a card that was inserted or edited since the bookmark is news; a Jira issue whose status moved, a rescheduled event. Arrivals: only a card that arrived since the bookmark; a mail is news once, when it comes. Each source picks its default; the policy file can override it.
