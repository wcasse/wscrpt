# RC65 invalidation notice

Date: 2026-07-28

Do not use `RC65.md`, `w-0.1.0-rc65-aarch64-apple-darwin`, or
`wscrpt-0.1.0-rc65.crate` as release provenance.

RC65 lost its immutable identity when a stale packaging process from an earlier
release attempt continued after the source tree had advanced and rewrote the
checkpoint artifacts. The initially observed manifest identified a binary hash
beginning `fcf780` and ending `aa839`, and a crate hash beginning `a622dfe` and
ending `40dac`. Later observations found different artifact bytes, including a
crate hash beginning `50d43649` and ending `7ac18`. At 18:38 PDT the RC65
manifest itself was rewritten to match yet another later artifact pair:

- binary SHA-256:
  `170154b943dc3fc9504aa550091e380503e6f9a1a27e495667a8a8227bcfe470`
- crate SHA-256:
  `3d6935db6674eb67725d2403c07622a398c904483c26273e31de336176689669`

Making the mutable manifest match the last overwrite does not restore the
original one-shot checkpoint identity. RC65 is therefore quarantined rather
than repaired or reused.

The two preceding checkpoints were re-hashed after this incident and remained
unchanged:

- RC63 binary:
  `f658ab44c546fc309d827f35dece46203cc4df2a1fd55a2067c35dc1cc35464a`
- RC63 crate:
  `52e798a75f2b96ba1b8b0e85b2d51fab19c1fae76e289c750478a2756a816172`
- RC64 binary:
  `a5018e97e7115b4e1ff1aab37f12a466594491f19b53a5f8231b11f3bea37f91`
- RC64 crate:
  `3389ea4ee0a15547896b12138415a7085ce9e1b3271dd43c7692330701c5ce48`

Use a later checkpoint whose artifacts were copied, hashed, documented, and
re-verified only after all source and validation work completed.
