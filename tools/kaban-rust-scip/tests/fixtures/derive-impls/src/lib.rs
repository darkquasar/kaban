pub trait Marker { fn m(&self) {} }
// r3-1: a cfg-gated derive before X occupies a slot in every context, so X keeps one identity
#[cfg(test)]
#[derive(derive_macros::Marked)]
pub struct T0;
#[derive(derive_macros::Marked)]
pub struct X;
#[derive(derive_macros::Marked)]
pub struct Z;
// r3-2: a derive under cfg_attr is a derive
#[cfg_attr(all(), derive(derive_macros::Marked))]
pub struct V;
pub struct Y;
const _: () = { impl Marker for Y {} };
pub struct W<T>(pub T);
const _: () = { impl<T> Marker for W<T> {} };
pub fn use_all() { X.m(); Y.m(); }
// r3-3: a call from a skipped helper to a published derived member publishes no occurrence
pub fn outer() { fn inner() { X.m(); } inner(); }
// r3-4: an accepted impl whose self type is external (or an alias to one, or a qualified local path)
const _: () = { impl Marker for std::time::Duration {} };
pub type Instant = std::time::Instant;
const _: () = { impl Marker for Instant {} };
pub struct Q;
const _: () = { impl Marker for crate::Q {} };
// consumer request 3: an impl whose only parameters are lifetimes is unconditional (serde's impl<'de> Deserialize<'de>)
pub trait Marker2<'a> {}
const _: () = { impl<'a> Marker2<'a> for X {} };
// fixg-1: a lifetime predicate keeps an impl conditional: an inline bound, a lifetime where-clause, a bound the trait
// declares (rustc E0478 makes the impl restate it inline); an impl with a concrete lifetime and no parameters is unconditional
pub struct X2;
const _: () = { impl<'a: 'static> Marker2<'a> for X2 {} };
pub struct X3;
const _: () = { impl<'a> Marker2<'a> for X3 where 'a: 'static {} };
pub trait Marker3<'a: 'static> {}
pub struct X4;
const _: () = { impl<'a: 'static> Marker3<'a> for X4 {} };
pub struct X5;
const _: () = { impl Marker3<'static> for X5 {} };
// fixg-2: sibling derives under one cfg_attr, the first test-only: the second keeps one identity in both contexts
pub trait MarkerB { fn b(&self) {} }
#[cfg_attr(all(), cfg_attr(test, derive(derive_macros::Marked)), derive(derive_macros::Marked2))]
pub struct N;
// fixg-3: an anonymous const under a pass-through attribute macro: its impl, member and edges are published
pub struct P;
#[derive_macros::passthrough]
const _: () = { impl Marker for P { fn m(&self) {} } };
