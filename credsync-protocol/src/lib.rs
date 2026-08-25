//! Wire types, canonical codec, checksums and scope digests for the credSync protocol.
//!
//! This crate is the single definition of what goes on the wire. [`docs/spec.md`] is its
//! specification and takes precedence: any wire change updates the spec, the types, the codec
//! and the fixtures **in the same pull request**, never separately.
//!
//! # Canonical encoding
//!
//! Encoding is *canonical*: the same logical value always produces identical bytes. This is
//! load-bearing rather than cosmetic, because checksums and scope digests are computed over the
//! encoding — instability there manifests as phantom corruption reports on real devices.
//!
//! # Status
//!
//! Scaffolded at CS-1. Types arrive at CS-3, checksums and digests at CS-4, fixtures at CS-5.
//!
//! [`docs/spec.md`]: https://github.com/TheAfricanDreamLab/credsync/blob/main/docs/spec.md

#![forbid(unsafe_code)]
