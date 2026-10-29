pub struct Nominal;
pub trait Behavior { fn act(&self); }
pub fn outer() {
    { impl Behavior for Nominal { fn act(&self) {} } }
}
