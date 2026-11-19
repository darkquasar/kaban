//! Bytes that keep an owner alive (spec 11 PC-8): [`retain_owner`] attaches a value, such as a
//! pin's lock, to an `OwnedBytes` span, so the value lives until the last clone or slice of the
//! returned bytes is dropped.

use std::ops::Deref;

use ownedbytes::OwnedBytes;
use stable_deref_trait::StableDeref;

/// `bytes` with `owner` attached: `owner` is dropped only after the last clone or slice of the
/// result is dropped, on whichever thread drops it. The content is `bytes`' own; nothing is
/// copied.
#[must_use]
pub fn retain_owner(bytes: OwnedBytes, owner: impl Send + Sync + 'static) -> OwnedBytes {
    OwnedBytes::new(Retained {
        bytes,
        _owner: owner,
    })
}

/// One span and its owner, the holder every clone and slice of [`retain_owner`]'s result
/// shares. Fields drop in order: the span before its owner.
struct Retained<O> {
    bytes: OwnedBytes,
    _owner: O,
}

impl<O> Deref for Retained<O> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.bytes.as_slice()
    }
}

// SAFETY: `deref` returns the span of `bytes`, which points into memory owned by `bytes`' own
// shared holder (a heap buffer or a file mapping), never into `Retained` itself, so moving a
// `Retained` does not move it. `bytes` is private, set once at construction and never replaced
// (the holder is only reachable through shared references once `OwnedBytes::new` owns it), and
// `deref` reads no other state, so every call returns the same span, valid until the
// `Retained` is dropped.
unsafe impl<O> StableDeref for Retained<O> {}
