# Architecture Decision Record — nmea0183

**Crate:** nmea0183
**Status:** Accepted

---

This file records decisions specific to the `nmea0183` library crate. Workspace-level
and `talker`-application decisions live in [`talker/docs/ADR.md`](../../talker/docs/ADR.md).
Several of those still touch `nmea0183` (its extraction as a separate crate in ADR-001,
the `thiserror` error model in ADR-004, the inline XOR checksum in ADR-007, the workspace
MSRV in ADR-008) — see that file for the reasoning. ADR numbers are shared across both
files and never reused, so cross-references stay valid.

---

## ADR-009 — Talker ID and sentence type extensibility in `nmea0183`

**Context:** NMEA 0183 has ~36 standard talker IDs and many sentence types. New proprietary sentences (`$P...`) are encountered regularly in marine and survey equipment.

**Decision:**
- Standard talker IDs are represented as an enum with a `Custom(String)` variant for arbitrary two-character IDs.
- Sentence types follow the same pattern: an enum with a `Custom(String)` variant.
- Proprietary sentences use a dedicated `ProprietarySentence` type with named variants for known formats (`Prdid`, `Pashr`) and a `Raw` variant for arbitrary `$P` sentences.

**Consequences:**
- Named proprietary sentences (`$PRDID`, `$PASHR`) get field-level construction and validation.
- The `Raw` variant accepts any manufacturer code and comma-separated field string with optional checksum — no validation beyond checksum computation.
- The `$PASHR` GNSS quality field (field 10) is exposed as a raw `u8` rather than an enum, because Trimble and Novatel define the values differently. The crate documentation must record both vendor conventions explicitly.
- `$PRDID` does not include a checksum by protocol convention; the builder must not append one.

---

## ADR-028 — Sentence types own explicit live UTC field metadata

**Status:** Accepted 2026-07-17.

**Context:** Applications that refresh NMEA time/date fields need protocol knowledge:
the same logical instant occupies different zero-based field positions in GGA, GLL,
RMC, ZDA, and related sentences. Keeping that table in a GUI or sender would duplicate
NMEA semantics and let future consumers disagree.

**Decision:** `SentenceType::time_fields()` returns a static slice of
`(field_index, TimeFieldKind)` entries. `TimeFieldKind` distinguishes UTC time,
`ddmmyy`, day, month, and four-digit year. The mapping is explicit, never inferred
from mnemonic names: ASHR/BWC/BWR/GBS/GGA/GNS/GRS/GST/ZFO/ZTG use UTC field 0; GLL
uses UTC field 4; RMC uses UTC field 0 and date field 8; ZDA uses fields 0–3 for UTC,
day, month, and year. Every other standard or custom type returns an empty slice.
Positions are relative to `NmeaSentence::fields`, excluding talker and mnemonic.

`TimeFieldKind` is deliberately exhaustive: a new formatting kind must make consumers
handle it at compile time. The existing extensible `SentenceType::Custom` boundary is
unchanged.

**Consequences:** NMEA field knowledge lives in the publishable library and Talker
only supplies an instant and formatting policy. An empty mapping means substitution
is unsupported, not that a sentence can never contain time-like data. Unit tests pin
every supported entry, sorted unique positions, and empty custom/unsupported types.

## ADR-030 — UTC field formatting normalizes leap-second components

**Status:** Accepted 2026-07-17.

**Context:** Talker live fields and Listener ZDA annotations both formatted NMEA UTC
time from clock components. Their duplicate format strings inherited an edge case
from Chrono's leap-second representation: `second()` remains 59 while subsecond
milliseconds may be 1000..=1999, and a minimum-width `{:03}` formatter then emits an
invalid four-digit fraction. Depending on Chrono here would make the independently
publishable protocol crate heavier for one component conversion.

**Decision:** `format_utc_time` is a dependency-free NMEA helper over numeric clock
components. Normal milliseconds emit `hhmmss[.sss]`. A Chrono-style leap component
(`second == 59`, subsecond milliseconds 1000..=1999) emits second `60` and subtracts
1000 from the fraction, preserving the field width and the represented instant. Both
applications use this one formatter; component-range preconditions are documented and
debug-asserted because their clock libraries already guarantee them.

**Consequences:** Talker and Listener cannot drift on NMEA UTC precision or leap
normalization, and `nmea0183` gains no clock dependency. Tests pin ordinary and leap
forms in the library and at both application boundaries.

---

## ADR-058 — Parsing is strict about framing and permissive about field content

**Status:** Accepted 2026-09-30.

**Context:** A review found framing gaps in the three parsers. A multibyte
character at the talker/type boundary made `NmeaSentence::parse` panic on a
byte-offset split, which the typed error model (ADR-004) exists to prevent. The
checksum suffix was read with `u8::from_str_radix`, which accepts a sign and
leading zeros, so `*0034` and `*+34` both passed as 0x34. An explicit empty field
was lost: `$GPHDT,*63` parsed back with no fields, so a built sentence and its
parsed copy differed. AIS fill bits and fragment numbers were accepted outside
the values the envelope can mean. The library specification is still a
placeholder, so this ADR is the parsing contract until it exists.

**Decision:**

- Input containing a non-ASCII character is rejected with an error before any
  slicing. NMEA 0183 sentences are ASCII.
- A checksum suffix is exactly two hexadecimal digits, in either case. The public
  `checksum::from_hex` follows the same rule.
- A comma after the header starts the field list even when nothing follows it:
  `$GPHDT,*63` parses to one empty field, and a header with no comma to none.
  Standard and raw proprietary sentences round-trip through construction and
  parsing.
- AIS fill bits are 0–5, the fragment count is at least 1, and the fragment
  number runs from 1 to the count.

**Boundary:** Field content stays permissive. Construction still accepts any
field text, including characters that break the framing, because talker uses
that for negative testing. Parsing validates no field values beyond the `$PRDID`
and `$PASHR` fields it already checked. Talker IDs and sentence types keep
ADR-009's extensibility. Strict constructors, fuzzing and the library
specification wait for a publication date.

**Alternatives considered:**

- **Check only the character boundary before the split:** Rejected. It removes
  the panic but still accepts a non-ASCII sentence, which no NMEA device emits.
- **Keep lenient checksum parsing:** Rejected. No device writes `*+34`; accepting
  it only widens what corrupt input can pass for valid.

**Consequences:** Every parser returns an error for input it cannot represent,
never a panic. A sentence with an empty field reads back as it was built. Only
tests in the workspace call the parsers — listener decodes nothing (listener
ADR-010) and talker only constructs — so no application behaviour changes.

---

## Open questions

**OQ-4 — `nmea0183` library MSRV policy.** ADR-008 sets the workspace MSRV to current stable Rust. The `nmea0183` library, intended for crates.io publication, may benefit from a looser MSRV to accommodate cautious downstream users. The policy (e.g., N-6 months of stable releases) and the mechanism (per-crate `rust-version` override) are deferred to a future ADR when publication approaches. See ADR-008 (in `talker/docs/ADR.md`) for context.
