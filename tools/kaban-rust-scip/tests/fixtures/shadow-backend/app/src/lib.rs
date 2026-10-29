#![forbid(unsafe_code)]

qualification_macros::make_value!();

pub fn answer() -> usize {
    qualification_support::base_value() + generated_value()
}
