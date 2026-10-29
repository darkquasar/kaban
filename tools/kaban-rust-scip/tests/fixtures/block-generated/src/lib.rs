macro_rules! inner { () => { fn unused_generated() {} }; }
macro_rules! nested { () => { { inner!(); } }; }
pub fn outer() { nested!(); }
