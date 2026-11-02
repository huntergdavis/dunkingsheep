//! Python-compatible Unicode default case folding for identity comparisons.
//!
//! The table is generated from the source Python runtime with
//! `tools/generate_unicode_casefold.py`. Unlisted scalars fold to themselves.

#[path = "unicode_casefold_table.rs"]
mod table;

pub(crate) fn full_casefold(value: &str) -> String {
    let mut folded = String::with_capacity(value.len());
    for scalar in value.chars() {
        append_casefold(scalar, &mut folded);
    }
    folded
}

fn append_casefold(scalar: char, output: &mut String) {
    match table::CASEFOLD.binary_search_by_key(&(scalar as u32), |(codepoint, _)| *codepoint) {
        Ok(index) => output.push_str(table::CASEFOLD[index].1),
        Err(_) => output.push(scalar),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn casefold_matches_python_special_casing_examples() {
        assert_eq!("strasse", full_casefold("Straße"));
        assert_eq!("οσ", full_casefold("ΟΣ"));
        assert_eq!("οσ", full_casefold("ος"));
        assert_eq!("ffi", full_casefold("ﬃ"));
        assert_eq!("i", full_casefold("I"));
        assert_eq!("i\u{307}", full_casefold("İ"));
        assert_eq!("ı", full_casefold("ı"));
        assert_eq!("ascii", full_casefold("ASCII"));
    }

    #[test]
    fn generated_mapping_matches_python_oracle_for_every_unicode_scalar() {
        assert_eq!("3.14.4", table::PYTHON_VERSION);
        assert_eq!("16.0.0", table::UNICODE_VERSION);
        let mut digest = Sha256::new();
        let mut folded = String::with_capacity(12);
        for codepoint in 0..=0x10_FFFF {
            let Some(scalar) = char::from_u32(codepoint) else {
                continue;
            };
            folded.clear();
            append_casefold(scalar, &mut folded);
            digest.update(folded.as_bytes());
            digest.update([0]);
        }
        let actual: String = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(table::ORACLE_SHA256, actual);
    }
}
