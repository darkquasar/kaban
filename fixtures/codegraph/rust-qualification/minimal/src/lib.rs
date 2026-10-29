#![forbid(unsafe_code)]

/// A driver with two implementations and a deliberately unrelated type.
pub trait DatabaseDriver {
    fn execute(&self) -> usize;
}

pub struct Postgres;
pub struct Sqlite;
pub struct NotADriver;

impl DatabaseDriver for Postgres {
    fn execute(&self) -> usize {
        production()
    }
}

impl DatabaseDriver for Sqlite {
    fn execute(&self) -> usize {
        2
    }
}

impl Postgres {
    pub fn new() -> Self {
        Self
    }
}

pub fn production() -> usize {
    middle()
}

pub fn middle() -> usize {
    leaf()
}

pub fn leaf() -> usize {
    1
}

pub fn function_value() -> fn() -> usize {
    leaf
}

pub fn café() -> usize {
    let local = "é";
    local.len() + std::mem::size_of::<usize>()
}

#[cfg(test)]
mod tests {
    #[test]
    fn production_works() {
        assert_eq!(super::production(), 1);
    }
}

pub fn fresh() -> Postgres {
    Postgres::new()
}
