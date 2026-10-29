//! Confusables skeleton for literal matching (core-spec S2; m2 N3).
//!
//! A small, deterministic table folds the Cyrillic, Greek and a few Latin look-alike letters to
//! their ASCII skeleton so that `Рroject` (Cyrillic Er) matches the literal `Project`. Fullwidth
//! and other compatibility forms are already handled by NFKD before this fold runs. The full
//! UTS #39 `confusables.txt` table is a documented upgrade path (it would replace [`fold`]
//! without changing any caller).

/// Look-alike letter -> ASCII skeleton. Both cases are listed so [`skeleton`] works on raw text;
/// the literal views apply [`fold`] after casefolding and only meet the lowercase half.
const TABLE: &[(char, &str)] = &[
    ('Ø', "O"), // U+00D8
    ('ø', "o"), // U+00F8
    ('Đ', "D"), // U+0110
    ('đ', "d"), // U+0111
    ('ı', "i"), // U+0131
    ('Ł', "L"), // U+0141
    ('ł', "l"), // U+0142
    ('ƅ', "b"), // U+0185
    ('ɡ', "g"), // U+0261
    ('ɩ', "i"), // U+0269
    ('ʀ', "r"), // U+0280
    ('Α', "A"), // U+0391
    ('Β', "B"), // U+0392
    ('Ε', "E"), // U+0395
    ('Ζ', "Z"), // U+0396
    ('Η', "H"), // U+0397
    ('Ι', "I"), // U+0399
    ('Κ', "K"), // U+039A
    ('Μ', "M"), // U+039C
    ('Ν', "N"), // U+039D
    ('Ο', "O"), // U+039F
    ('Ρ', "P"), // U+03A1
    ('Τ', "T"), // U+03A4
    ('Υ', "Y"), // U+03A5
    ('Χ', "X"), // U+03A7
    ('α', "a"), // U+03B1
    ('β', "b"), // U+03B2
    ('γ', "y"), // U+03B3
    ('ε', "e"), // U+03B5
    ('η', "n"), // U+03B7
    ('ι', "i"), // U+03B9
    ('κ', "k"), // U+03BA
    ('ν', "v"), // U+03BD
    ('ο', "o"), // U+03BF
    ('ρ', "p"), // U+03C1
    ('τ', "t"), // U+03C4
    ('υ', "u"), // U+03C5
    ('χ', "x"), // U+03C7
    ('ω', "w"), // U+03C9
    ('Ё', "E"), // U+0401
    ('Є', "E"), // U+0404
    ('Ѕ', "S"), // U+0405
    ('І', "I"), // U+0406
    ('Ї', "I"), // U+0407
    ('Ј', "J"), // U+0408
    ('А', "A"), // U+0410
    ('В', "B"), // U+0412
    ('Е', "E"), // U+0415
    ('К', "K"), // U+041A
    ('М', "M"), // U+041C
    ('Н', "H"), // U+041D
    ('О', "O"), // U+041E
    ('Р', "P"), // U+0420
    ('С', "C"), // U+0421
    ('Т', "T"), // U+0422
    ('У', "Y"), // U+0423
    ('Х', "X"), // U+0425
    ('а', "a"), // U+0430
    ('в', "b"), // U+0432
    ('г', "r"), // U+0433
    ('е', "e"), // U+0435
    ('к', "k"), // U+043A
    ('м', "m"), // U+043C
    ('н', "h"), // U+043D
    ('о', "o"), // U+043E
    ('р', "p"), // U+0440
    ('с', "c"), // U+0441
    ('т', "t"), // U+0442
    ('у', "y"), // U+0443
    ('х', "x"), // U+0445
    ('ё', "e"), // U+0451
    ('є', "e"), // U+0454
    ('ѕ', "s"), // U+0455
    ('і', "i"), // U+0456
    ('ї', "i"), // U+0457
    ('ј', "j"), // U+0458
    ('Ѵ', "V"), // U+0474
    ('ѵ', "v"), // U+0475
    ('Ү', "Y"), // U+04AE
    ('ү', "y"), // U+04AF
    ('һ', "h"), // U+04BB
    ('Ө', "O"), // U+04E8
    ('ө', "o"), // U+04E9
    ('ԁ', "d"), // U+0501
    ('ԅ', "s"), // U+0505
    ('Ԍ', "G"), // U+050C
    ('ԍ', "g"), // U+050D
    ('Ԛ', "Q"), // U+051A
    ('ԛ', "q"), // U+051B
    ('Ԝ', "W"), // U+051C
    ('ԝ', "w"), // U+051D
];

/// ASCII skeleton of one char, or `None` when the char is not a known confusable.
pub(crate) fn fold(c: char) -> Option<&'static str> {
    TABLE
        .binary_search_by_key(&c, |(k, _)| *k)
        .ok()
        .map(|i| TABLE[i].1)
}

/// Fold every known confusable in `s` to its ASCII skeleton. Deterministic; chars outside the
/// table are copied unchanged.
pub fn skeleton(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match fold(c) {
            Some(k) => out.push_str(k),
            None => out.push(c),
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_and_unique_for_binary_search() {
        for w in TABLE.windows(2) {
            assert!(
                w[0].0 < w[1].0,
                "table must be sorted: {:?} >= {:?}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn skeleton_folds_cyrillic_and_greek_to_ascii() {
        assert_eq!(skeleton("\u{0420}roject"), "Project");
        assert_eq!(skeleton("ρassword"), "password");
        assert_eq!(skeleton("plain ascii"), "plain ascii");
    }
}
