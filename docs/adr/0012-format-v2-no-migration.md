# ADR-0012: On-disk format v2, no migration

Status: accepted (M2.1)

M2.1 extends `Message` with `Delete` and `Upsert` variants, changing the
bincode encoding inside node records, so `format_version` is bumped to 2
everywhere. `open()` on a v1 file fails with a typed `UnsupportedVersion`
error — distinguishable from "not a database" because the slot still
authenticates by magic and CRC. There is deliberately NO migration: the
project is pre-release, no v1 data exists outside its own test suites, and
a migration path written now would be dead code to maintain against a
format that is still moving. The version gate is the contract; migrations
become worth their cost the day a release exists.
