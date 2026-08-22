//! What the parts of a series are called, in the languages this crate reads names in.
//!
//! Top-level rather than tucked under [`crate::page`], because it is *data about language* and
//! carries no knowledge of pages, downloads or yt-dlp. Anything that has to read a human-written
//! name can use it.
//!
//! # Grouped by language, and that is the point
//!
//! A flat list of episode words and another of season words would happily pair a Spanish season
//! with an English episode, and a name that needed two languages to be understood has almost
//! certainly been misread. Keeping each language's words together means a season is only ever
//! folded into an episode number when *one* language named both.
//!
//! # The shorthand is not a language
//!
//! `S03E12` is a filename convention, not English — it appears on Japanese and Spanish pages alike.
//! So it is its own [`Vocabulary`], listed last: `e` is the weakest evidence in here and should lose
//! to any name that spells the word out.
//!
//! # Where the number sits
//!
//! Most of these put the number after the word (`Episode 12`, `פרק 12`, `Épisode 12`). Japanese and
//! Chinese count with a character *after* it — `第12話`, `第3季` — so a marker may be looked for on
//! either side of its digits. Russian does both in practice (`Серия 5` and `5 серия`), which the
//! same rule covers for free.
//!
//! # Single letters
//!
//! `é`, `ע` and `פ` are here because those languages really do abbreviate that far, and they are
//! only safe because a word has to *begin* at the match — otherwise the `é` in `café 3` would read
//! as an episode. Russian is the deliberate omission: `с` abbreviates both серия and сезон and is
//! also the everyday preposition "with", which is ambiguous once too often.

/// One language's words for the parts of a series.
pub(crate) struct Vocabulary {
    /// What to call it in a message, and what tells two vocabularies apart.
    pub(crate) language: &'static str,
    /// Words saying the number beside them is an episode's.
    pub(crate) episode: &'static [&'static str],
    /// Words saying the number beside them is a season's.
    pub(crate) season: &'static [&'static str],
}

/// Every language a series' parts are recognised in, strongest evidence first.
///
/// Unaccented spellings sit beside accented ones because pages routinely drop the diacritic, and a
/// missing accent should not cost a whole numbering. Matching is case-insensitive, so everything
/// here is lower case.
pub(crate) const SERIES: &[Vocabulary] = &[
    Vocabulary {
        language: "English",
        episode: &["episode"],
        season: &["season"],
    },
    Vocabulary {
        language: "Spanish",
        episode: &["episodio", "capítulo", "capitulo"],
        season: &["temporada"],
    },
    Vocabulary {
        language: "French",
        episode: &["épisode", "episode", "ép", "é"],
        season: &["saison"],
    },
    Vocabulary {
        language: "Italian",
        episode: &["episodio", "puntata"],
        season: &["stagione"],
    },
    // No single-letter Russian abbreviation: `с` would be the obvious one for both серия and
    // сезон, and it is also the everyday preposition "with". Ambiguous twice over is too much.
    Vocabulary {
        language: "Russian",
        episode: &["эпизод", "серия", "эп"],
        season: &["сезон", "сез"],
    },
    Vocabulary {
        language: "Hebrew",
        episode: &["פרק", "פ"],
        season: &["עונה", "ע"],
    },
    Vocabulary {
        language: "Japanese",
        episode: &["エピソード", "話"],
        season: &["シーズン", "期"],
    },
    Vocabulary {
        language: "Chinese",
        episode: &["集"],
        season: &["季"],
    },
    // Last on purpose: see the module header. `e` and `s` are the vaguest things in here, so a name
    // that spells its words out must be preferred over one that doesn't.
    Vocabulary {
        language: "shorthand",
        episode: &["episode", "ep", "e"],
        season: &["season", "se", "s"],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_vocabulary_is_usable_and_lower_case() {
        assert!(!SERIES.is_empty());
        for vocabulary in SERIES {
            assert!(!vocabulary.language.is_empty());
            // A vocabulary with no episode word can never number anything, and one with no season
            // word is fine — but an entirely empty one is a mistake.
            assert!(!vocabulary.episode.is_empty(), "{} names no episode", vocabulary.language);
            for word in vocabulary.episode.iter().chain(vocabulary.season) {
                assert!(!word.is_empty(), "{} has an empty word", vocabulary.language);
                assert_eq!(
                    *word,
                    word.to_lowercase(),
                    "{}: matching lower-cases the name, so `{word}` could never match",
                    vocabulary.language
                );
            }
        }
    }

    #[test]
    fn the_shorthand_is_last_so_a_spelled_out_word_always_wins() {
        // `e` is the weakest evidence here and `episode` the strongest. Order is what stops the
        // former being consulted while the latter would have answered.
        let last = SERIES.last().expect("a vocabulary");
        assert_eq!(last.language, "shorthand");
        assert!(last.episode.contains(&"e"), "the vague ones live in the shorthand entry");
        assert!(last.season.contains(&"s"));
        // A lone *Latin letter* is shorthand and belongs in that entry. A lone CJK character is a
        // whole word — 話 and 季 are not abbreviations of anything.
        for vocabulary in &SERIES[..SERIES.len() - 1] {
            for word in vocabulary.episode.iter().chain(vocabulary.season) {
                assert!(
                    !(word.chars().count() == 1 && word.chars().all(|c| c.is_ascii_alphabetic())),
                    "{}: `{word}` is a single Latin letter and belongs in the shorthand entry",
                    vocabulary.language
                );
            }
        }
    }

    #[test]
    fn a_words_longest_form_is_listed_before_the_forms_it_contains() {
        // `ep` must not be tried before `episode`: it would match the same text, then fail to find
        // digits where the longer word would have found them. So if one word is a prefix of
        // another, the longer has to come first.
        for vocabulary in SERIES {
            for words in [vocabulary.episode, vocabulary.season] {
                for (at, earlier) in words.iter().enumerate() {
                    for later in &words[at + 1..] {
                        assert!(
                            !later.starts_with(earlier),
                            "{}: `{later}` is longer than `{earlier}` and must be listed first",
                            vocabulary.language
                        );
                    }
                }
            }
        }
    }
}
