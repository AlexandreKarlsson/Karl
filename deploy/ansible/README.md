# Ansible role skeleton

The role actually used is `minipc/ansible/roles/karl` in the setup repository
(`../setup`): it renders `karl.toml` from `karl_sources` / `karl_profiles`,
`secrets.toml` from `vault_karl_secrets`, the egress table and the unit, and
the Hermes role there mounts the CLI and the socket into each profile's
sandbox. What is here is the shape, kept for readers of the docs; copy the
setup repo's role if you start from scratch.
