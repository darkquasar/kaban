#![allow(non_upper_case_globals)]
pub struct S { pub x: u32 }
impl S { pub const x: u32 = 1; pub type Unit = (); }

#[cfg(not(test))]
mod production { impl super::S { pub fn m(&self) -> u32 { self.x } } }
#[cfg(test)]
mod testing { impl super::S { pub fn m(&self) -> u32 { 2 } } }

pub fn use_members(s: &S) -> u32 { s.m() + S::x }
