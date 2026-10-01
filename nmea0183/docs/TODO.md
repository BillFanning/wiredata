# TODO — nmea0183

Work the `nmea0183` library does not do yet. Decisions go in [ADR.md](ADR.md);
workspace- and `talker`-level tasks live in
[`talker/docs/TODO.md`](../../talker/docs/TODO.md).

Remove an item when it is done: the commit says what changed.

---

## Before publishing `nmea0183` to crates.io

- [ ] Add publication metadata to `nmea0183/Cargo.toml`:
  - `repository = "..."`
  - `documentation = "..."` (or rely on docs.rs default)
  - `readme = "README.md"`
  - `keywords = ["nmea", "nmea0183", "marine", "gnss", "gps"]` (max 5)
  - `categories = ["parser-implementations", "encoding"]` (must match crates.io category slugs)
- [ ] Write `nmea0183/README.md`.
- [ ] Resolve OQ-4 (library MSRV policy) in a new ADR.
- [ ] Update `talker/Cargo.toml`: `nmea0183 = { path = "../nmea0183", version = "0.1" }`
  (per OQ-1), so downstream builds against the published crate resolve while
  in-workspace builds use the local source.
- [ ] Flesh out `nmea0183_specification.md`. It is still a **placeholder** that
  defers behaviour to rustdoc, tests and ADRs. AGENTS §1 ranks a crate's spec
  **above** its ADR, so an empty spec weakens that precedence for this crate.
  Either write the spec (sentence set, checksum, talker IDs, armoring rules) or
  amend AGENTS §1 to say nmea0183's behaviour is defined by rustdoc and tests by
  design.
