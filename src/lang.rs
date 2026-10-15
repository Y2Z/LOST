//! Page languages, for `lang:` / `language:` (ARCHITECTURE.md §7.1).
//!
//! A page's language is guessed from its text when it's indexed, with
//! `whatlang` (70 languages, trigram statistics, no data files). It isn't
//! stored with the page or in `.lost` files: it's worked out again from the
//! text whenever the index is built, so better detection later needs no
//! change anywhere else. Languages are kept as ISO 639-3 codes (`deu`), and
//! `lang:` takes any of the usual ways to write one: `de`, `deu`, `ger`,
//! `german`, `deutsch`.

/// How much of a page is read to tell its language: enough to be sure,
/// little enough not to slow imports down.
const SAMPLE_CHARS: usize = 1_000;

/// The language of a page, as an ISO 639-3 code, or `None` when its text is
/// too short or too mixed to tell.
pub fn detect(title: Option<&str>, body: &str) -> Option<&'static str> {
    let mut sample = String::with_capacity(SAMPLE_CHARS * 2);
    if let Some(title) = title {
        sample.push_str(title);
        sample.push('\n');
    }
    sample.extend(body.chars().take(SAMPLE_CHARS));
    let info = whatlang::detect(&sample)?;
    info.is_reliable().then(|| info.lang().code())
}

/// Other ways to write the languages `whatlang` knows: ISO 639-1 codes,
/// ISO 639-2/B codes, and common names that aren't its own. Its codes
/// (ISO 639-3) and its English and native names work too, without being
/// listed here.
const ALIASES: &[(&str, &[&str])] = &[
    ("afr", &["af"]),
    ("aka", &["ak"]),
    ("amh", &["am"]),
    ("ara", &["ar"]),
    ("aze", &["az"]),
    ("bel", &["be"]),
    ("ben", &["bn", "bangla"]),
    ("bul", &["bg"]),
    ("cat", &["ca"]),
    ("ces", &["cs", "cze"]),
    ("cmn", &["zh", "zho", "chi", "chinese"]),
    ("cym", &["cy", "wel"]),
    ("dan", &["da"]),
    ("deu", &["de", "ger"]),
    ("ell", &["el", "gre"]),
    ("eng", &["en"]),
    ("epo", &["eo"]),
    ("est", &["et"]),
    ("fin", &["fi"]),
    ("fra", &["fr", "fre"]),
    ("guj", &["gu"]),
    ("heb", &["he", "iw"]),
    ("hin", &["hi"]),
    ("hrv", &["hr"]),
    ("hun", &["hu"]),
    ("hye", &["hy", "arm"]),
    ("ind", &["id", "in"]),
    ("ita", &["it"]),
    ("jav", &["jv"]),
    ("jpn", &["ja"]),
    ("kan", &["kn"]),
    ("kat", &["ka", "geo"]),
    ("khm", &["km"]),
    ("kor", &["ko"]),
    ("lat", &["la"]),
    ("lav", &["lv"]),
    ("lit", &["lt"]),
    ("mal", &["ml"]),
    ("mar", &["mr"]),
    ("mkd", &["mk", "mac"]),
    ("mya", &["my", "bur", "myanmar"]),
    ("nep", &["ne"]),
    ("nld", &["nl", "dut", "flemish"]),
    ("nob", &["nb", "no", "nor", "norwegian"]),
    ("ori", &["or", "odia"]),
    ("pan", &["pa", "panjabi"]),
    ("pes", &["fa", "fas", "per", "farsi"]),
    ("pol", &["pl"]),
    ("por", &["pt"]),
    ("ron", &["ro", "rum", "moldovan"]),
    ("rus", &["ru"]),
    ("sin", &["si", "sinhala"]),
    ("slk", &["sk", "slo"]),
    ("slv", &["sl", "slovenian"]),
    ("sna", &["sn"]),
    ("spa", &["es"]),
    ("srp", &["sr"]),
    ("swe", &["sv"]),
    ("tam", &["ta"]),
    ("tel", &["te"]),
    ("tgl", &["tl", "fil", "filipino"]),
    ("tha", &["th"]),
    ("tuk", &["tk"]),
    ("tur", &["tr"]),
    ("ukr", &["uk"]),
    ("urd", &["ur"]),
    ("uzb", &["uz"]),
    ("vie", &["vi"]),
    ("yid", &["yi", "ji"]),
    ("zul", &["zu"]),
];

/// The ISO 639-3 code of a language written as a code or a name, in any
/// case: `de`, `DEU`, `ger`, `German`, `Deutsch` all give `deu`. `None` for
/// a language LOST can't tell apart (it would match nothing).
pub fn code(name: &str) -> Option<&'static str> {
    let name = name.trim().to_lowercase();
    if name.is_empty() {
        return None;
    }
    for lang in whatlang::Lang::all() {
        if lang.code() == name
            || lang.eng_name().to_lowercase() == name
            || lang.name().to_lowercase() == name
        {
            return Some(lang.code());
        }
    }
    ALIASES
        .iter()
        .find(|(_, aliases)| aliases.contains(&name.as_str()))
        .map(|(code, _)| *code)
}

/// A language's English name, from its ISO 639-3 code.
pub fn english_name(code: &str) -> Option<&'static str> {
    whatlang::Lang::from_code(code).map(|lang| lang.eng_name())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_codes() {
        for name in ["de", "DEU", "ger", "German", "deutsch", " Deutsch "] {
            assert_eq!(code(name), Some("deu"), "{name}");
        }
        assert_eq!(code("русский"), Some("rus"));
        assert_eq!(code("zh"), Some("cmn"));
        assert_eq!(code("Chinese"), Some("cmn"));
        assert_eq!(code("Mandarin"), Some("cmn"));
        assert_eq!(code("no"), Some("nob"));
        assert_eq!(code("farsi"), Some("pes"));
        assert_eq!(code("klingon"), None);
        assert_eq!(code(""), None);
        assert_eq!(english_name("deu"), Some("German"));
        assert_eq!(english_name("xyz"), None);
    }

    #[test]
    fn every_language_has_its_short_code() {
        // Each of whatlang's languages can be written as a two-letter code,
        // and no alias points at a language it doesn't know.
        for lang in whatlang::Lang::all() {
            let listed = ALIASES.iter().find(|(c, _)| *c == lang.code());
            let short = listed.and_then(|(_, a)| a.iter().find(|a| a.len() == 2));
            assert!(short.is_some(), "{} has no two-letter code", lang.code());
        }
        for (c, _) in ALIASES {
            assert!(whatlang::Lang::from_code(*c).is_some(), "{c}");
        }
        // And no alias is taken twice.
        let mut all: Vec<&str> = ALIASES
            .iter()
            .flat_map(|(_, a)| a.iter().copied())
            .collect();
        let count = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), count);
    }

    #[test]
    fn detection() {
        assert_eq!(
            detect(
                Some("Das Wetter"),
                "Das ist ein ganz normaler deutscher Satz über das Wetter in Berlin und die Stadt."
            ),
            Some("deu")
        );
        assert_eq!(
            detect(
                None,
                "I2P is an anonymous network layer that allows for censorship-resistant, \
                 peer-to-peer communication between its users."
            ),
            Some("eng")
        );
        // Too little to tell.
        assert_eq!(detect(Some("Welcome"), ""), None);
        assert_eq!(detect(None, ""), None);
    }
}
