# Global device room metadata

The room belongs to the YAM device, not to any particular plugin, hardware model
or private installation. No room names are hard-coded: every user may choose a
label after installing YAM, or leave it unassigned. Changing a room never changes
model detection, hardware capabilities, mixer state or running plugins.

## Set or clear a room

- Web UI: **Room / Комната**, enter a label and save. Blank clears it.
- CLI: `yam room` reads; `yam room "Kitchen"` sets; `yam room --clear` clears.
- Management API: `GET /api/room`, `PUT /api/room` with JSON
  `{"room":"Kitchen"}` or `{"room":null}`. It uses the existing management API
  bind/access policy; do not expose an unauthenticated administrative API to WAN.

Leading/trailing whitespace is trimmed; empty becomes null. Labels may contain
up to 64 Unicode characters; control characters and unknown request fields are
rejected. Storage is atomic in `data-dir/room.toml`, separate from `device.toml`.
Restarting YAM or upgrading a plugin does not discard this device metadata.
Malformed saved metadata fails startup explicitly rather than silently losing it.

## Read from clients and plugins

`GET /api/device` adds `"room":"Kitchen"` (or null) to the existing device object;
all existing model/architecture/features/source fields keep their meanings.
Binary plugins receive the same optional field from token-authenticated
`GET /api/runtime/<id>/device` with the existing exec permission. Lua plugins read
`device.room` (nil when unset). A Lua invocation's device snapshot reflects the
room at launch; a long-running binary may re-query its runtime device endpoint.

No fake model, LED capability, plugin settings convention, environment variable
or extra receiver package is required. Consumers must accept a missing/null field
for compatibility with older YAM. Updating room does not automatically restart
services: a plugin which reads identity only on startup needs a normal restart.
AirPlay can use the room in its discovery name; other packages can use exactly
the same YAM field without depending on AirPlay.

## Validation

Room normalization/limits/clear, atomic persistence, independent hardware
identity and invalid-file handling have Rust unit tests. The standalone
`test-room-api.py DAEMON CLIENT` runs an isolated daemon (UI/updates/binary-exec
off, no packages, nonexistent LED paths) and checks HTTP, CLI, invalid requests,
room persistence across daemon restart, unchanged hardware and fail-closed load.
No sound, LED or mixer writes are required to test this feature.
