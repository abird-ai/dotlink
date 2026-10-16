# Agent docs

Plan status entry point: `../plans/START_HERE.md` and `../plans/TODO.md`.

- `configuration.md` — schema-v10 profile fields, OAuth/ngrok setup, transport precedence, path permissions, and cache behavior.
- `release-install.md` — release artifacts, installers, overrides, Nix/Crane outputs, and platform notes.
- `nixos-service.md` — NixOS module options, user-profile fallback, systemd credentials, multi-instance services, journald, and security.

Public-facing usage stays in the repository `README.md`; these files retain
implementation/reference detail without bloating the public README. The dated
plan bundles under `../plans/` preserve design history; their open work is
tracked as checklists, not handoff prose.
