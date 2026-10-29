pub fn leaf() -> u32 { 7 }

pub fn first() -> u32 {
    struct Reentrant(u32);
    impl Reentrant { fn get(&self) -> u32 { self.0 } }
    Reentrant(1).get()
}

pub fn second() -> u32 {
    struct Reentrant;
    fn descend() -> u32 { leaf() }
    let _marker = Reentrant;
    descend()
}

pub fn third() -> u32 {
    let café = leaf();
    assert_eq!(café, 7);
    café
}

macro_rules! helper { ($name:ident) => { fn $name() -> u32 { leaf() } }; }
pub fn fourth() -> u32 { helper!(generated); generated() }
