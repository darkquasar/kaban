pub fn outer() -> u32 {
    { fn inner() -> u32 { 7 } inner() }
}
