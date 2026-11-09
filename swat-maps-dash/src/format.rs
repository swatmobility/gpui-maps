//! Formatting helpers (spec §2.2 `format`). Phase 1 carries the NFKC
//! normaliser the `contains` operator binds through (§4.2.3a); the number
//! and date tokens of §3.7 arrive with the widgets.

use unicode_normalization::UnicodeNormalization;

/// Unicode NFKC, trimmed: what a `contains` input is bound as (§4.2.3a).
/// It does not case-fold. Pure Rust, so identical on wasm.
pub fn nfkc(s: &str) -> String {
    s.nfkc().collect::<String>().trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_nfkc() {
        assert_eq!(nfkc("ＡＢＣ"), "ABC");
        assert_eq!(nfkc("１２"), "12");
        assert_eq!(nfkc("ｱｵﾓﾘ"), "アオモリ");
        assert_eq!(nfkc("ｶﾞ"), "ガ");
        assert_eq!(nfkc("カ\u{3099}"), "ガ");
        assert_eq!(nfkc("  abc "), "abc", "trimmed, not case-folded");
        assert_eq!(nfkc("Abc"), "Abc");
    }
}
