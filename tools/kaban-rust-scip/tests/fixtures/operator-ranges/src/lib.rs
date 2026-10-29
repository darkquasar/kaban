macro_rules! identity { ($expression:expr) => { $expression }; }
pub fn ordinary(left: usize, right: usize) -> usize {
    left /* not an operator */ + right
}
pub fn expanded(left: usize, right: usize) -> usize {
    identity!(left  +  right)
}
pub fn ordinary_body() -> usize {
    #[cfg(any())]
    { fn inactive_item() {} }
    ordinary(1, 2)
}
