#[cfg(not(test))]
pub fn selected() -> usize { 1 }

#[cfg(test)]
pub fn selected() -> usize { 2 }
