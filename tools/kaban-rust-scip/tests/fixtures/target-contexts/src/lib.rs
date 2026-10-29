pub fn common() -> usize { 1 }

#[cfg(not(test))]
pub fn production_only() -> usize { common() }

#[cfg(test)]
mod tests {
    #[test]
    fn inline_test() { assert_eq!(super::common(), 1); }
}

macro_rules! generate {
    ($name:ident) => { pub fn $name() -> usize { 7 } };
}
generate!(unused_generated);
