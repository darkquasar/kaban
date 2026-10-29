pub trait Marker { fn mark(&self); }
pub struct Concrete { pub field: usize }
pub struct Wrapper<T>(pub T);
pub enum Choice { Alpha { value: u8 }, Beta }
impl Marker for Concrete { fn mark(&self) {} }
impl<T: Marker> Marker for Wrapper<T> { fn mark(&self) {} }
impl Marker for &Concrete { fn mark(&self) {} }
impl Marker for (Concrete,) { fn mark(&self) {} }
pub trait Blanket {}
impl<T> Blanket for T {}
pub trait Conditional {}
impl Conditional for Concrete where Concrete: Marker {}
pub struct TestOnly;
#[cfg(test)]
impl Marker for TestOnly { fn mark(&self) {} }
pub struct ProductionOnly;
#[cfg(not(test))]
impl Marker for ProductionOnly { fn mark(&self) {} }
macro_rules! generated { () => {
    pub fn unused_generated() {}
    pub struct UnusedGenerated;
}; }
generated!();
pub fn read(value: Concrete) -> usize {
    let field = value.field;
    let _variant = Choice::Alpha { value: 1 };
    field
}
// backlog 7: nominal tuple fields (a tuple struct's, a tuple variant's) have an identity; a plain tuple's `.0` has none
pub enum Pair { Both(u8, u8) }
pub fn tuple_fields(wrapper: Wrapper<u8>, pair: Pair, plain: (u8, u8)) -> u8 {
    let Pair::Both(left, _) = pair;
    wrapper.0 + left + plain.0
}
