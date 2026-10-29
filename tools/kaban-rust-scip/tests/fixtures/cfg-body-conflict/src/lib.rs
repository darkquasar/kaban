pub fn one() {}
pub fn two() {}
pub fn ambiguous_body() {
    #[cfg(not(test))] one();
    #[cfg(test)] two();
}
