pub fn always_built() -> u8 {
    1
}

// an anonymous item (the shape serde's derives expand to); it has no symbol of its own
const _: () = ();
