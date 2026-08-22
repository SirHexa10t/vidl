//! What a page calls the collection it lists.
//!
//! A page that lists a season names each episode, and the name of the season itself is usually not
//! written down anywhere — it is the part every episode's name has in common. So that is what this
//! module works out: the longest opening the videos share.
//!
//! # Why it is discovered rather than looked up
//!
//! Nothing here knows what the field is called. The page this was built against uses `name`; the
//! next one will use something else, and a table of field names per site is exactly the thing
//! `--multi` exists to avoid. So the field is *found*, by the property that makes it the
//! descriptive one:
//!
//! - it is present on every video the page lists,
//! - its values are all **different** — an id or a category label repeats, a title does not,
//! - and those values share the **longest opening** of any such field.
//!
//! On the page that prompted this, `name` wins outright: eighteen distinct values sharing a
//! 29-character opening that is precisely the series name and season number. `description` is also
//! present and also distinct, and shares nothing, so it loses on the third test.
//!
//! # What is still missing
//!
//! Choosing what to call the individual *files*. The platform names those well enough today — every
//! sibling on the page that prompted this came back with a full, distinct title of its own — so the
//! need hasn't arisen. When it does, the design is the same one this module already implements,
//! turned outward: print the fields found against the resolved video, let the caller name the one
//! they want as a flag, and apply that field to every sibling. Deliberately not a prompt: this
//! crate gives every child a null stdin and is driven from a library entry point, so an interactive
//! picker would break scripted and embedded use.
//!
//! # How solid this is
//!
//! A heuristic, and it says so. Objects are isolated by matching braces, which a `{` inside a
//! string value could throw off, and fields are read only at the object's own level so a name
//! nested deeper is invisible. Both were true of the page it was written against and neither is
//! guaranteed. The cost of being wrong is a poorly named folder, which is why it is allowed to
//! guess at all.

use std::collections::{HashMap, HashSet};

use crate::keywords;

/// The shortest shared opening worth naming a folder after. Below this it is a coincidence between
/// two titles rather than a collection's name.
const MIN_OPENING: usize = 4;

/// The opening that every one of `ids` shares, according to whatever field the page describes them
/// by. `None` when the page describes them by nothing they have in common.
pub(crate) fn shared_opening(page: &str, ids: &[String]) -> Option<String> {
    let values = described_by(page, ids)?;
    let opening = trim_dangling(common_opening(&values));
    (opening.len() >= MIN_OPENING).then_some(opening)
}

/// The number each video's own name ends with, in the order `ids` were given — but only when those
/// numbers really are a sequence.
///
/// This is what a collection should be numbered by when it has its own numbering. Page order is a
/// poor substitute for it: the page this was built against lists its episodes newest-first *and*
/// repeats the featured one at the top, so ordering by position numbered episode 8 as `001` and
/// episode 18 as `002`. Its names carry 1 to 18, which is the ordering that was meant.
///
/// The numbers are read from the same field [`described_by`] picked for the collection's name, so
/// the two can never disagree about which field describes the videos. That field is chosen by the
/// opening its values share, which means a page whose names share nothing may have some other field
/// picked — and the tests below are what stop such a field's numbers being used as episode numbers.
///
/// `None` unless every video yields a number and the numbers, sorted, run consecutively. Both
/// conditions matter: a missing one leaves a video unnumberable, a repeat would collide two files
/// on the same prefix, and requiring them to be consecutive is what keeps the page's *internal*
/// ids — which are distinct but scattered — from being mistaken for episode numbers.
pub(crate) fn sequence(page: &str, ids: &[String]) -> Option<Numbering> {
    // Each candidate field in turn, best first. A field that names the videos beautifully may carry
    // no numbers at all, and the field that carries them may be nobody's idea of a description.
    candidates(page, ids).iter().find_map(|values| {
        // A name that says which number is the episode is better evidence than any guess, so it is
        // asked first — and it sidesteps the stray entirely rather than detecting it afterwards.
        marked_sequence(values).or_else(|| guessed_sequence(values))
    })
}

/// The last number in each name, when they happen to run consecutively.
///
/// Nothing here knows these are episode numbers — only that they are distinct, complete, and in a
/// run, which is enough to be a better ordering than page position and not enough to be labelled
/// `E`. The consecutiveness is the whole guarantee: it is what keeps a page's internal row ids,
/// which are distinct but scattered, from being mistaken for an index.
fn guessed_sequence(values: &[&str]) -> Option<Numbering> {
    let numbers: Vec<usize> = values.iter().filter_map(|value| last_number(value)).collect();
    if numbers.len() != values.len() {
        return None;
    }
    let mut sorted = numbers.clone();
    sorted.sort_unstable();
    sorted.windows(2).all(|pair| pair[1] == pair[0] + 1).then(|| Numbering {
        prefixes: padded(&numbers),
        source: "the last number in each name".to_string(),
    })
}

/// A collection's own numbering, and what supplied it.
///
/// The source is worth carrying because the run says it out loud: "numbered by their Hebrew episode
/// numbers" tells a reader far more about whether to trust the result than a bare list of prefixes.
pub(crate) struct Numbering {
    /// One prefix per video, in the order the ids were given — already sortable as text, so a
    /// season can be written as itself (`S6E01`) instead of being folded into arithmetic.
    pub(crate) prefixes: Vec<String>,
    /// How they were arrived at, phrased to follow "numbering them by ".
    pub(crate) source: String,
}

/// How many digits `value` needs — the width every number in a set is padded to, so `S6E01` and
/// `S6E18` line up while a series of eight episodes stays at `S5E4`.
pub(crate) fn digits(value: usize) -> usize {
    value.checked_ilog10().unwrap_or(0) as usize + 1
}

/// `numbers` as labels of equal width, so they sort as text in the order they mean.
///
/// Undecorated, and only used where the number was *guessed* — the last run of digits in a name, or
/// a video's position on the page. Both are orderings this crate inferred rather than read, and
/// leaving them bare is what lets a leading letter mean "the name said so".
fn padded(numbers: &[usize]) -> Vec<String> {
    let width = digits(numbers.iter().copied().max().unwrap_or(0));
    numbers.iter().map(|number| format!("{number:0width$}")).collect()
}

/// [`padded`], behind the letter naming what the number is: `E01`, `S4`.
fn labelled(letter: &str, numbers: &[usize]) -> Vec<String> {
    padded(numbers).iter().map(|number| format!("{letter}{number}")).collect()
}

/// The numbers a set of names give *by name* — where each says which of its numbers is the episode.
///
/// This is the trustworthy path. A name reading `Episode 5: The 3 Musketeers` has two numbers in it
/// and only one of them is the episode; anchoring on the word is what tells them apart, where
/// taking the last run of digits would take the stray. So no shape test is imposed on the result
/// beyond the numbers being distinct — they don't have to prove they aren't row ids, because the
/// name already said what they are.
///
/// A season, when *every* name carries one, is written into the label rather than folded into
/// arithmetic: season 5 episode 4 becomes `S5E04`, season 5 episode 11 `S5E11`. Both parts are
/// indexes, and a label saying so sorts exactly as well as a number while staying readable. Each
/// part is padded to the width its own set needs, so a season of eight episodes stays at `S5E4`.
///
/// Used only when every name carries a season, since mixing `S6E17` with a bare `17` would order
/// them absurdly.
fn marked_sequence(values: &[&str]) -> Option<Numbering> {
    let mut episode_only: Option<Numbering> = None;
    for vocabulary in keywords::SERIES {
        // The episode comes first and nothing proceeds without it: a season on its own numbers
        // nothing, and a label reading `S4` for every video would sort them all together.
        let found: Vec<(usize, usize)> =
            values.iter().filter_map(|value| marked_span(value, vocabulary.episode)).collect();
        if found.len() != values.len() {
            continue;
        }
        let episodes: Vec<usize> = found.iter().map(|(number, _)| *number).collect();
        let named = |what: &str| format!("their {} {what} numbers", vocabulary.language);
        // Then the season, on top — from the same language, and never off the *same digits*. A name
        // shaped `<number>: <title>` has one number in it, and reading it as both parts would give
        // every video an `S3E3`-style label that says nothing.
        let seasons: Vec<usize> = values
            .iter()
            .zip(&found)
            .filter_map(|(value, (_, episode_at))| {
                marked_span(value, vocabulary.season)
                    .filter(|(_, season_at)| season_at != episode_at)
                    .map(|(number, _)| number)
            })
            .collect();
        // The episode label is built first and stands on its own; the season, when there is one,
        // is written in front of it. `E12` alone is a complete answer — and it says the name told
        // us so, where a bare `12` could equally be a position we counted.
        let episode_labels = labelled("E", &episodes);
        if seasons.len() == values.len() {
            // One language named both parts, off different digits. Written out as `S6E01` rather
            // than folded into a number: both parts are indexes, and a label that says so sorts
            // exactly as well as arithmetic while staying readable.
            let both: Vec<String> = labelled("S", &seasons)
                .iter()
                .zip(&episode_labels)
                .map(|(season, episode)| format!("{season}{episode}"))
                .collect();
            if let Some(prefixes) = all_distinct(both) {
                return Some(Numbering { prefixes, source: named("season and episode") });
            }
        }
        if episode_only.is_none() {
            if let Some(prefixes) = all_distinct(episode_labels) {
                episode_only = Some(Numbering { prefixes, source: named("episode") });
            }
        }
    }
    episode_only
}

/// `labels` when no two are the same — two files cannot share one prefix.
fn all_distinct(labels: Vec<String>) -> Option<Vec<String>> {
    let unique = labels.iter().collect::<HashSet<_>>().len();
    (unique == labels.len()).then_some(labels)
}

/// The number introduced by the last of `markers` that actually introduces one.
///
/// A word has to sit at a boundary, and the two reading directions need that tested differently —
/// forwards it must begin a word, backwards it must end one. Either way the *edge of the name* is a
/// boundary as good as a separator: `Episode 12` reads, and so does `12話`.
///
/// Between the word and its digits any run of space, dot, hyphen or underscore is allowed, or none
/// at all — which is what makes `S3E12`, `Ep. 12` and `episode-12` the same thing.
/// The number a word introduces, **and where those digits sit**.
///
/// The position is what lets a caller refuse to read one run of digits as two different things —
/// see [`marked_sequence`], where a name like `期3話` would otherwise report season 3 and episode 3
/// off the same `3`.
fn marked_span(text: &str, markers: &[&str]) -> Option<(usize, usize)> {
    let lower = text.to_lowercase();
    let mut found = None;
    for at in lower.char_indices().map(|(at, _)| at) {
        for marker in markers {
            if !lower[at..].starts_with(marker) {
                continue;
            }
            let rest = &lower[at + marker.len()..];
            let after_letter = lower[..at].chars().next_back().is_some_and(char::is_alphabetic);
            let before_letter = rest.chars().next().is_some_and(char::is_alphabetic);

            // Forward order — the word introduces its number, so it must BEGIN a word. The test
            // is "not preceded by a letter", which is *not* the same as "preceded by a non-letter":
            // nothing at all satisfies it too, so a name opening with its word still reads. Without
            // the test, the `ep` in `1 step 2` and the `é` in `café 3` both become episodes. And
            // only a word that then leads to digits counts, so `e` failing inside `episode` doesn't
            // stop the longer word from being tried.
            if !after_letter {
                if let Some((number, skipped)) = number_after(rest) {
                    found = Some((number, at + marker.len() + skipped));
                    break;
                }
            }

            // Reverse order — the number comes first and the word counts it: 第12話, 5 серия.
            // Its boundary is the mirror image, so the word must END one, and again the end of the
            // name counts as much as a separator does — which is the ordinary case, since nothing
            // usually follows a counter. `сезон` inside `сезонов` is not a count of anything, and
            // neither is the `ע` inside a Hebrew word. Scripts written without spaces are exempt,
            // because for them there is no word end to find: in 第3季第12集 the season's counter is
            // followed immediately by the episode's.
            //
            // Latin shorthand never reads this way round, or a name ending `2024 s` becomes a
            // season. The leading boundary needs no test here: what precedes is a digit.
            if !marker.is_ascii() && (spaceless(marker) || !before_letter) {
                if let Some((number, start)) = number_before(&lower[..at]) {
                    found = Some((number, start));
                    break;
                }
            }
        }
    }
    found
}

/// Whether `word` is written in a script that runs without spaces, so there is no word end for a
/// counter to sit at. Japanese and Chinese count that way — `第3季第12集` puts one counter straight
/// against the next — where a Cyrillic or Hebrew word has an end and must be at it.
fn spaceless(word: &str) -> bool {
    word.chars().all(|character| {
        matches!(character as u32,
            0x3040..=0x30FF   // hiragana and katakana
            | 0x3400..=0x4DBF // CJK extension A
            | 0x4E00..=0x9FFF // CJK unified ideographs
            | 0xF900..=0xFAFF // CJK compatibility ideographs
        )
    })
}

/// The number `text` ends with, and where those digits begin — past any separator between them and
/// whatever followed them.
fn number_before(text: &str) -> Option<(usize, usize)> {
    let trimmed = text.trim_end_matches([' ', '.', '-', '_']);
    let backwards: String = trimmed.chars().rev().take_while(|c| c.is_ascii_digit()).collect();
    let run: String = backwards.chars().rev().collect();
    let start = trimmed.len() - run.len();
    run.parse().ok().map(|number| (number, start))
}

/// The number `rest` opens with, and how far into `rest` its digits begin — past any separator
/// between them and whatever introduced them.
fn number_after(rest: &str) -> Option<(usize, usize)> {
    let digits = rest.trim_start_matches([' ', '.', '-', '_']);
    let skipped = rest.len() - digits.len();
    let run: String = digits.chars().take_while(|c| c.is_ascii_digit()).collect();
    run.parse().ok().map(|number| (number, skipped))
}

/// The field the page describes these videos by, and its value for each of them in `ids` order.
///
/// One choice serves both the collection's name and its numbering, so the two can't disagree about
/// which field is the descriptive one. Every id must be found: a missing one would shift the values
/// out of step with the videos they belong to, which numbering cannot survive.
fn described_by<'a>(page: &'a str, ids: &[String]) -> Option<Vec<&'a str>> {
    candidates(page, ids).into_iter().next()
}

/// Every field the page gives *all* of these videos with distinct values, best first.
///
/// "Best" is the longest opening its values share, which is what makes a field the descriptive one —
/// so the first entry is what names the collection. The rest are kept because numbering is a
/// different question: names shaped `3: Some Name` share no opening at all, so the field that names
/// them would lose to any field that happens to, and its numbers would never be looked at.
///
/// Ties break on the field's own name, so the order is the same on every run. Left to a hash map's
/// iteration order it would not be.
fn candidates<'a>(page: &'a str, ids: &[String]) -> Vec<Vec<&'a str>> {
    if ids.len() < 2 {
        return Vec::new();
    }
    let described: Vec<HashMap<&str, &str>> = ids.iter().map(|id| fields_for(page, id)).collect();
    if described.iter().any(HashMap::is_empty) {
        return Vec::new();
    }
    // Only fields the page gives *every* video: one missing from a single entry can't describe the
    // set.
    let mut shared: HashSet<&str> = described[0].keys().copied().collect();
    for fields in &described[1..] {
        shared.retain(|key| fields.contains_key(key));
    }
    let mut ranked: Vec<(usize, &str, Vec<&str>)> = shared
        .iter()
        .filter_map(|key| {
            let values: Vec<&str> = described.iter().map(|fields| fields[*key]).collect();
            // An id or a category label repeats itself; a title doesn't. This is what tells the
            // descriptive field from a `typeDescription` of "Episode" on every entry.
            let distinct: HashSet<&&str> = values.iter().collect();
            let opening = trim_dangling(common_opening(&values)).len();
            (distinct.len() == values.len()).then_some((opening, *key, values))
        })
        .collect();
    ranked.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(right.1)));
    ranked.into_iter().map(|(_, _, values)| values).collect()
}

/// The last run of adjacent digits in `text`, as a number.
///
/// That is the whole rule, and saying it that way is the point: a run ends at anything that isn't a
/// digit, so there is no list of separators to enumerate and no case to overlook. `S3E12` gives 12
/// because `E` isn't a digit, for the same reason `part_12` and `Episode 12` do.
///
/// The *last* run, because a name carries its series and season first: `A Show, Season 6, Episode
/// 17` must yield 17.
///
/// Two things it will not do. Digits means *ASCII* digits, since those are what a number parses
/// from — a name numbered in another script yields nothing, and the collection falls back to page
/// order. And if that last run is too long to be a `usize`, the answer is nothing rather than an
/// earlier run: the last run is the rule, and a rule that quietly reaches backwards isn't one.
fn last_number(text: &str) -> Option<usize> {
    let mut last = None;
    let mut start = None;
    for (at, character) in text.char_indices() {
        if character.is_ascii_digit() {
            start.get_or_insert(at);
        } else if let Some(from) = start.take() {
            last = Some(&text[from..at]);
        }
    }
    if let Some(from) = start {
        last = Some(&text[from..]);
    }
    // A run too long to be a number is not one; `parse` saying so is the answer.
    last?.parse().ok()
}

/// The longest opening common to every one of `values`, cut at a character boundary.
fn common_opening<'a>(values: &[&'a str]) -> &'a str {
    let first = values.first().copied().unwrap_or_default();
    let mut end = 0;
    for (at, _) in first.char_indices().skip(1).chain(std::iter::once((first.len(), ' '))) {
        if values.iter().all(|value| value.as_bytes().starts_with(first[..at].as_bytes())) {
            end = at;
        } else {
            break;
        }
    }
    &first[..end]
}

/// An opening with its trailing joinery removed.
///
/// Two things are shed. The obvious one is trailing punctuation and space. The subtler one is a
/// final *word*: an opening that stops after a bare space has usually run one word past what the
/// videos really share, into the label that introduces the difference — `…, Season 6, Episode `
/// before the number. An opening that stops after punctuation has not, because the punctuation is
/// itself where the shared part ended, so `A Show - ` keeps its `Show`.
///
/// The word is only dropped when something is left afterwards: `Entry ` is the whole name, not a
/// label.
fn trim_dangling(opening: &str) -> String {
    let mut kept = opening;
    let joinery: String = opening.chars().rev().take_while(|c| !c.is_alphanumeric()).collect();
    if !joinery.is_empty() && joinery.chars().all(char::is_whitespace) {
        let body = opening.trim_end();
        if let Some(at) = body.rfind(char::is_whitespace) {
            if !bare(&body[..at]).is_empty() {
                kept = &body[..at];
            }
        }
    }
    bare(kept).to_string()
}

/// `text` without its trailing punctuation or space.
fn bare(text: &str) -> &str {
    text.trim_end_matches(|c: char| !c.is_alphanumeric()).trim()
}

/// Every flat string field the page carries about `id`, merged across each place it mentions it.
///
/// Merged on purpose: the featured video is usually written twice, once in the player's
/// configuration and once in the list, and only the second carries a name. Taking either occurrence
/// alone would drop the descriptive field out of the set shared by every video.
fn fields_for<'a>(page: &'a str, id: &str) -> HashMap<&'a str, &'a str> {
    let mut merged = HashMap::new();
    let mut from = 0;
    while let Some(at) = page[from..].find(id) {
        let offset = from + at;
        if let Some(object) = object_around(page, offset) {
            for (key, value) in flat_fields(object) {
                merged.entry(key).or_insert(value);
            }
        }
        from = offset + id.len();
    }
    merged
}

/// The `{…}` enclosing `offset`, matched by counting braces outward.
fn object_around(page: &str, offset: usize) -> Option<&str> {
    let bytes = page.as_bytes();
    let mut depth = 0usize;
    let mut open = None;
    for at in (0..offset).rev() {
        match bytes[at] {
            b'}' => depth += 1,
            b'{' if depth == 0 => {
                open = Some(at);
                break;
            }
            b'{' => depth -= 1,
            _ => {}
        }
    }
    let open = open?;
    let mut depth = 0usize;
    for at in open..bytes.len() {
        match bytes[at] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return page.get(open..=at);
                }
            }
            _ => {}
        }
    }
    None
}

/// The `"key":"value"` pairs at `object`'s own level, skipping anything nested inside it.
///
/// Only this level: a field one object deeper belongs to that object, not this one, and following
/// it down is how a generic reader starts guessing.
fn flat_fields(object: &str) -> Vec<(&str, &str)> {
    let bytes = object.as_bytes();
    let mut fields = Vec::new();
    let (mut depth, mut at) = (0usize, 0usize);
    // A string is a key until a colon says the next one is its value.
    let mut key: Option<&str> = None;
    let mut after_colon = false;
    while at < bytes.len() {
        match bytes[at] {
            b'{' | b'[' => {
                depth += 1;
                (key, after_colon) = (None, false);
                at += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                (key, after_colon) = (None, false);
                at += 1;
            }
            b'"' => {
                let Some(end) = string_end(bytes, at) else { break };
                let text = &object[at + 1..end];
                // Depth 1 is the object's own level: past its opening brace, inside nothing else.
                if depth == 1 {
                    if after_colon {
                        if let Some(named) = key.take() {
                            fields.push((named, text));
                        }
                        after_colon = false;
                    } else {
                        key = Some(text);
                    }
                }
                at = end + 1;
            }
            b':' => {
                after_colon = true;
                at += 1;
            }
            b',' => {
                (key, after_colon) = (None, false);
                at += 1;
            }
            _ => at += 1,
        }
    }
    fields
}

/// The index of the closing quote of the string starting at `open`, honouring backslash escapes.
fn string_end(bytes: &[u8], open: usize) -> Option<usize> {
    let mut at = open + 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'"' => return Some(at),
            _ => at += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of the page this was built against: a list of entries, each described by a `name`
    /// that opens with the series and season, plus fields that repeat and fields that share nothing.
    fn episodes(names: &[&str], ids: &[&str]) -> String {
        let mut page = String::from(r#"{"playerConfig":{"entryId":"#);
        page.push_str(&format!("\"{}\"}},\"episodes\":[", ids[0]));
        for (name, id) in names.iter().zip(ids) {
            page.push_str(&format!(
                r#"{{"id":123,"name":"{name}","description":"see notes",
                   "entryId":"{id}","typeDescription":"Episode",
                   "tags":{{"nested":{{"name":"NOT THIS ONE"}}}}}},"#
            ));
        }
        page.push_str("]}");
        page
    }

    #[test]
    fn the_collection_is_named_by_the_opening_its_videos_share() {
        // Eighteen-ish entries whose names differ only after the season: exactly the page this was
        // written for, and the answer is the part they hold in common.
        let names = ["A Show, Season 6, Episode 1", "A Show, Season 6, Episode 2",
                     "A Show, Season 6, Episode 3"];
        let ids = ["1_aaaaaaaa", "1_bbbbbbbb", "1_cccccccc"];
        let page = episodes(&names, &ids);
        let owned: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
        assert_eq!(shared_opening(&page, &owned).as_deref(), Some("A Show, Season 6"));
        // The label that introduced the difference is gone, not left dangling.
        assert!(!shared_opening(&page, &owned).unwrap().ends_with("Episode"));
    }

    #[test]
    fn a_field_that_repeats_is_not_the_descriptive_one() {
        // `typeDescription` is identical on every entry, so its "shared opening" is the whole value
        // and would win on length. Requiring the values to be DISTINCT is what rules it out.
        let names = ["Show S1E1", "Show S1E2"];
        let ids = ["1_aaaaaaaa", "1_bbbbbbbb"];
        let page = episodes(&names, &ids);
        let owned: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
        let found = shared_opening(&page, &owned);
        assert_eq!(found.as_deref(), Some("Show S1E"), "the name, not the category label");
        assert_ne!(found.as_deref(), Some("Episode"), "`typeDescription` repeats, so it is out");
        assert_ne!(found.as_deref(), Some("see notes"), "and so does the boilerplate description");
    }

    #[test]
    fn videos_whose_names_share_nothing_get_no_collection_name() {
        let names = ["Entirely unrelated", "Something else again"];
        let ids = ["1_aaaaaaaa", "1_bbbbbbbb"];
        let page = episodes(&names, &ids);
        let owned: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
        // Nothing distinct shares an opening: the description and the category label repeat, so
        // they are excluded, and the entry ids share only "1_" which is below the floor.
        assert_eq!(shared_opening(&page, &owned), None);

        let plain = r#"{"episodes":[{"name":"Alpha","entryId":"1_aaaaaaaa"},
                                    {"name":"Zulu","entryId":"1_bbbbbbbb"}]}"#;
        assert_eq!(shared_opening(plain, &owned), None);
    }

    #[test]
    fn a_name_nested_deeper_belongs_to_that_object_and_not_to_this_one() {
        // The fixture buries a `name` inside a `tags` sub-object. Reading it would be a generic
        // reader starting to guess; fields are taken at the entry's own level only.
        let page = episodes(&["Show S1E1", "Show S1E2"], &["1_aaaaaaaa", "1_bbbbbbbb"]);
        let fields = fields_for(&page, "1_bbbbbbbb");
        assert_eq!(fields.get("name"), Some(&"Show S1E2"), "the entry's own name is read");
        assert!(
            !fields.values().any(|value| *value == "NOT THIS ONE"),
            "a nested name must not be: {fields:?}"
        );
        // And the merge across occurrences works: the featured entry is written twice, once in the
        // player configuration with no name at all, and both are folded into one view of it.
        let featured = fields_for(&page, "1_aaaaaaaa");
        assert_eq!(featured.get("name"), Some(&"Show S1E1"), "found in the list, not the config");
    }

    /// The ids the fixtures use, as `sequence` and `shared_opening` want them.
    fn owned(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    /// Just the number a word introduces, dropping where its digits sat.
    fn marked_number(text: &str, markers: &[&str]) -> Option<usize> {
        marked_span(text, markers).map(|(number, _)| number)
    }

    /// One language's words, by name — for exercising `marked_number` on its own.
    fn words(language: &str) -> &'static keywords::Vocabulary {
        keywords::SERIES.iter().find(|v| v.language == language).expect("a listed language")
    }

    /// The prefixes a page yields, joined — so an assertion reads as the ordering it expects.
    fn labels(page: &str, ids: &[&str]) -> Option<String> {
        sequence(page, &owned(ids)).map(|found| found.prefixes.join(","))
    }

    #[test]
    fn a_collection_is_numbered_by_the_numbers_in_its_own_names() {
        // The real shape: the page lists newest-first, so its order and the true order disagree.
        // The numbers come back in *page* order — they are what each video is called, and the
        // caller pairs them with the sibling it is downloading.
        let ids = ["1_aaaaaaaa", "1_bbbbbbbb", "1_cccccccc"];
        let page = episodes(&["A Show, Episode 3", "A Show, Episode 2", "A Show, Episode 1"], &ids);
        assert_eq!(labels(&page, &ids).as_deref(), Some("E3,E2,E1"));

        // A run that doesn't start at one is still a run — the back half of a season, say.
        let later = episodes(&["Ep 12", "Ep 13", "Ep 14"], &ids);
        assert_eq!(labels(&later, &ids).as_deref(), Some("E12,E13,E14"));
    }

    #[test]
    fn the_number_is_the_last_one_and_the_separator_can_be_anything() {
        // Last, because the series and season come first and must not win.
        assert_eq!(last_number("A Show, Season 6, Episode 17"), Some(17));
        assert_eq!(last_number("2024 retrospective 5"), Some(5));
        // The rule is just "the last run of adjacent digits", so what separates the runs never
        // needs enumerating. A letter does it as well as punctuation or a space.
        assert_eq!(last_number("S3E12"), Some(12), "a letter separates the runs");
        assert_eq!(last_number("S01E07"), Some(7));
        assert_eq!(last_number("S12E3"), Some(3), "the shorter run still wins if it is last");
        assert_eq!(last_number("part-12"), Some(12));
        assert_eq!(last_number("part_12"), Some(12));
        assert_eq!(last_number("ep.9"), Some(9));
        assert_eq!(last_number("Episode 4"), Some(4));
        assert_eq!(last_number("1x14"), Some(14));
        assert_eq!(last_number("padded 007"), Some(7), "leading zeros are only zeros");
        assert_eq!(last_number("Episode 12 (final)"), Some(12), "trailing words are not digits");
        assert_eq!(last_number("42"), Some(42), "a name that is only a number");
        // Nothing to take, and something too long to be a number.
        assert_eq!(last_number("Trailer"), None);
        assert_eq!(last_number(""), None);
        assert_eq!(last_number(&format!("huge {}", "9".repeat(40))), None);
    }

    #[test]
    fn a_name_that_says_which_number_is_the_episode_is_believed_over_the_last_one() {
        // The case that motivated this: two numbers in the name, and the stray is last. Anchoring
        // on the word takes the right one where "last run of digits" takes the wrong one.
        assert_eq!(marked_number("Episode 5: The 3 Musketeers", words("English").episode), Some(5));
        assert_eq!(last_number("Episode 5: The 3 Musketeers"), Some(3), "which is why the word wins");

        // Every spelling, any case, with or without a separator.
        for name in ["Episode 12", "episode-12", "Ep 12", "EP.12", "ep_12", "E12", "e 12"] {
            assert_eq!(marked_number(name, words("shorthand").episode), Some(12), "{name}");
        }
        for name in ["Season 5", "season-5", "SE.5", "se_5", "S5", "s 5"] {
            assert_eq!(marked_number(name, words("shorthand").season), Some(5), "{name}");
        }
    }

    #[test]
    fn a_season_and_episode_are_read_in_each_language_that_names_them() {
        // One case per language, each written the way that language writes it. Japanese and Chinese
        // count with a character *after* the number, which is why a word may be looked for on
        // either side of its digits.
        let ids = ["1_aaaaaaaa", "1_bbbbbbbb"];
        for (language, first, second) in [
            ("English", "Season 3, Episode 12", "Season 3, Episode 2"),
            ("Spanish", "Temporada 3, Episodio 12", "Temporada 3, Episodio 2"),
            ("French", "Saison 3, Épisode 12", "Saison 3, Épisode 2"),
            ("Italian", "Stagione 3, Episodio 12", "Stagione 3, Episodio 2"),
            ("Russian", "Сезон 3, Серия 12", "Сезон 3, Серия 2"),
            ("Hebrew", "עונה 3, פרק 12", "עונה 3, פרק 2"),
            ("Japanese", "第3期第12話", "第3期第2話"),
            ("Chinese", "第3季第12集", "第3季第2集"),
            ("shorthand", "S03E12", "S03E02"),
        ] {
            let page = episodes(&[first, second], &ids);
            assert_eq!(labels(&page, &ids).as_deref(), Some("S3E12,S3E02"), "{language}: {first}");
        }
    }

    #[test]
    fn a_language_that_counts_after_the_number_is_read_that_way_round() {
        // The counter follows its digits in Japanese and Chinese, and Russian writes it both ways.
        assert_eq!(marked_number("第12話", words("Japanese").episode), Some(12));
        assert_eq!(marked_number("第12集", words("Chinese").episode), Some(12));
        assert_eq!(marked_number("第3季", words("Chinese").season), Some(3));
        assert_eq!(marked_number("12 серия", words("Russian").episode), Some(12));
        assert_eq!(marked_number("Серия 12", words("Russian").episode), Some(12));
        // Latin shorthand is deliberately *not* read that way round: a name ending "2024 s" would
        // otherwise be read as season 2024.
        assert_eq!(marked_number("2024 s", words("shorthand").season), None);
        assert_eq!(marked_number("12 e", words("shorthand").episode), None);
    }

    #[test]
    fn a_season_is_never_borrowed_from_another_language() {
        // A Spanish season beside an English episode is a name nobody wrote, and reading it that
        // way would invent a number. The episode alone is the honest answer.
        let ids = ["1_aaaaaaaa", "1_bbbbbbbb"];
        let mixed = episodes(&["Temporada 3, Episode 12", "Temporada 3, Episode 2"], &ids);
        assert_eq!(labels(&mixed, &ids).as_deref(), Some("E12,E02"), "no season from two languages");
    }

    #[test]
    fn a_collection_with_no_season_named_is_numbered_by_its_episodes_alone() {
        // The season is optional throughout, and this is what its absence produces: bare padded
        // episode numbers, with no `E` and nothing invented in place of the season.
        let ids = ["1_aaaaaaaa", "1_bbbbbbbb", "1_cccccccc"];
        let page = episodes(&["Episode 3", "Episode 12", "Episode 7"], &ids);
        let found = sequence(&page, &owned(&ids)).expect("episodes alone are enough");
        assert_eq!(found.prefixes, ["E03", "E12", "E07"], "the episode label stands alone");
        // And the run says so: the source names the episode and does not claim a season.
        assert!(found.source.contains("episode"), "{}", found.source);
        assert!(!found.source.contains("season"), "nothing to say about seasons: {}", found.source);

        // With a season named, the same names gain the other half — this is the contrast.
        let seasoned = episodes(&["Season 4, Episode 3", "Season 4, Episode 12"], &ids[..2]);
        let found = sequence(&seasoned, &owned(&ids[..2])).expect("both parts");
        assert_eq!(found.prefixes, ["S4E03", "S4E12"]);
        assert!(found.source.contains("season and episode"), "{}", found.source);
    }

    #[test]
    fn one_run_of_digits_is_never_read_as_both_the_season_and_the_episode() {
        let ids = ["1_aaaaaaaa", "1_bbbbbbbb"];
        // The shape that worried us: a name whose number is all it has. No word names it, so the
        // guesswork answers and the label stays bare — nothing invents a season out of the same
        // digits.
        let three = ["1_aaaaaaaa", "1_bbbbbbbb", "1_cccccccc"];
        let bare = episodes(&["1: Pilot", "2: Second", "3: Third"], &three);
        assert_eq!(labels(&bare, &three).as_deref(), Some("1,2,3"), "guessed, so undecorated");

        // And the sandwiched case, where a season word and an episode word really do sit on either
        // side of one number. Reading it as both would give every video an `S3E3`-shaped label that
        // says nothing; comparing where the digits sat is what refuses it.
        assert_eq!(marked_number("期3話", words("Japanese").season), Some(3), "read on its own…");
        assert_eq!(marked_number("期3話", words("Japanese").episode), Some(3), "…both find it");
        let sandwiched = episodes(&["期3話", "期12話"], &ids);
        assert_eq!(
            labels(&sandwiched, &ids).as_deref(),
            Some("E03,E12"),
            "the episode stands, and no season is taken from its digits"
        );
    }

    #[test]
    fn the_edge_of_the_name_counts_as_a_boundary_in_both_directions() {
        // The condition is "not preceded by a letter", which nothing at all satisfies just as well
        // as a separator does. Both branches lean on that, and a later "fix" requiring an actual
        // preceding character would break every name that opens with its word.
        for (name, expected) in [("Episode 12", 12), ("E12", 12), ("ep.12", 12)] {
            assert_eq!(marked_number(name, words("shorthand").episode), Some(expected), "{name}");
        }
        assert_eq!(marked_number("S3", words("shorthand").season), Some(3), "nothing before it");
        assert_eq!(marked_number("פרק 5", words("Hebrew").episode), Some(5));
        assert_eq!(marked_number("エピソード12", words("Japanese").episode), Some(12));

        // And the mirror: read backwards, the *end* of the name is a boundary too, which is the
        // ordinary case for a counter — nothing follows it.
        assert_eq!(marked_number("12話", words("Japanese").episode), Some(12));
        assert_eq!(marked_number("12 серия", words("Russian").episode), Some(12));
    }

    #[test]
    fn a_word_read_forwards_has_to_begin_a_word() {
        // `ep` inside `step` is not an episode marker. This is the rule that keeps the short
        // abbreviations usable at all — without it every `1 step 2` reads as episode 2.
        for name in ["1 step 2 jumps", "1 steps 2", "a deep 3"] {
            assert_eq!(marked_number(name, words("shorthand").episode), None, "{name}");
        }
        // A word start that leads nowhere is no match either: `ep` begins a word in `1 epic 2`,
        // but what follows it is a letter, not a number.
        assert_eq!(marked_number("1 epic 2", words("shorthand").episode), None);
        // And the same rule protects the accented single letters.
        assert_eq!(marked_number("café 3", words("French").episode), None);
        assert_eq!(marked_number("Été 3", words("French").episode), None);
        // While the genuine abbreviation still reads.
        assert_eq!(marked_number("2 ep 3", words("shorthand").episode), Some(3));
        assert_eq!(marked_number("é 3", words("French").episode), Some(3));
    }

    #[test]
    fn a_word_read_backwards_has_to_end_one_unless_its_script_has_no_ends() {
        // The mirror rule, and it needs to be separate: what precedes a counter is a digit, so the
        // *leading* test passes for free and only the trailing one does any work.
        assert_eq!(marked_number("12 серия", words("Russian").episode), Some(12));
        // `сезон` inside `сезонов` counts nothing — that is a description, not an index.
        assert_eq!(marked_number("5 сезонов", words("Russian").season), None);
        // Nor is the `ע` inside a Hebrew word.
        assert_eq!(marked_number("3 עמוד", words("Hebrew").season), None);
        assert_eq!(marked_number("3 ע", words("Hebrew").season), Some(3));

        // Japanese and Chinese are exempt, because they have no word end to sit at: here one
        // counter runs straight into the next.
        assert_eq!(marked_number("第3季第12集", words("Chinese").season), Some(3));
        assert_eq!(marked_number("第3季第12集", words("Chinese").episode), Some(12));
        assert!(spaceless("話") && spaceless("季") && spaceless("シーズン"));
        assert!(!spaceless("сезон") && !spaceless("ע") && !spaceless("episode"));

        // Latin shorthand is never read backwards at all: `2024 s` is a year, not a season.
        assert_eq!(marked_number("2024 s", words("shorthand").season), None);
    }

    #[test]
    fn a_marker_has_to_begin_a_word() {
        // The `e` inside an ordinary word is not an episode marker, or every "Volume 3" would be
        // read as an episode. These fall through to the guesswork instead, as they should.
        for name in ["Volume 3", "Phase 2", "Take 5", "Line 9"] {
            assert_eq!(marked_number(name, words("shorthand").episode), None, "{name}");
        }
        // And a season marker must not be found inside a word that merely starts with one.
        assert_eq!(marked_number("Series 4", words("shorthand").season), None);
        assert_eq!(marked_number("Show 3", words("shorthand").season), None);
    }

    #[test]
    fn a_season_is_folded_in_ahead_of_the_episode() {
        let ids = ["1_aaaaaaaa", "1_bbbbbbbb"];
        // The stated shape: season 5 episode 4 is 504, season 5 episode 11 is 511.
        let page = episodes(&["S05E04", "S05E11"], &ids);
        assert_eq!(labels(&page, &ids).as_deref(), Some("S5E04,S5E11"), "the stated shape");
        // Compact and spelled-out forms are the same thing.
        let spelled = episodes(&["Season 3, Episode 12", "Season 3, Episode 2"], &ids);
        assert_eq!(labels(&spelled, &ids).as_deref(), Some("S3E12,S3E02"));
        // Each part padded to the width its own set needs, so a short season stays short.
        let short = episodes(&["S5E4", "S5E8"], &ids);
        assert_eq!(labels(&short, &ids).as_deref(), Some("S5E4,S5E8"), "no needless zero");
        // And the labels sort as text in the order they mean, which is the whole point of them.
        let mut sorted: Vec<String> =
            labels(&page, &ids).unwrap().split(',').map(str::to_string).collect();
        sorted.sort();
        assert_eq!(sorted, ["S5E04", "S5E11"]);

        // A season only counts when every name carries one — mixing 312 with a bare 2 would order
        // them absurdly, so the episode numbers stand alone instead. (The names still share an
        // opening, because that is what identifies them as the descriptive field in the first
        // place; see `described_by`.)
        let mixed = episodes(&["A Show, Season 3, Episode 12", "A Show, Episode 2"], &ids);
        assert_eq!(labels(&mixed, &ids).as_deref(), Some("E12,E02"));
    }

    #[test]
    fn marked_numbers_need_not_be_consecutive_because_the_name_vouched_for_them() {
        // The consecutiveness test exists to catch numbers that might not be episode numbers at
        // all. A name saying `Episode` has already settled that, so gaps are allowed — a page
        // listing a selection of episodes is still numbered by them.
        let ids = ["1_aaaaaaaa", "1_bbbbbbbb", "1_cccccccc"];
        let gapped = episodes(&["Episode 1", "Episode 5", "Episode 9"], &ids);
        assert_eq!(labels(&gapped, &ids).as_deref(), Some("E1,E5,E9"));
        // A repeat is still refused: two files cannot share one prefix.
        let repeated = episodes(&["Episode 1", "Episode 1 again", "Episode 9"], &ids);
        assert_eq!(labels(&repeated, &ids), None);
    }

    #[test]
    fn numbers_that_are_not_a_sequence_are_refused() {
        let ids = ["1_aaaaaaaa", "1_bbbbbbbb", "1_cccccccc"];
        // One video with no number of its own cannot be numbered by one.
        let missing = episodes(&["Ep 1", "Ep 2", "Trailer"], &ids);
        assert_eq!(labels(&missing, &ids), None);
        // A repeat would put two files on the same prefix.
        let repeated = episodes(&["Ep 1", "Ep 2", "Ep 2 revisited"], &ids);
        assert_eq!(labels(&repeated, &ids), None);
        // Distinct but scattered: these are the page's own row ids, not episode numbers, and this
        // is the case the consecutiveness test exists to reject.
        let internal = episodes(&["Row 2025892", "Row 2025894", "Row 2026744"], &ids);
        assert_eq!(labels(&internal, &ids), None);
        // And one video is not a sequence to find.
        assert!(sequence(&missing, &owned(&["1_aaaaaaaa"])).is_none(), "one video is no sequence");
    }

    #[test]
    fn one_video_is_not_a_collection_to_name() {
        let page = episodes(&["Only one"], &["1_aaaaaaaa"]);
        assert_eq!(shared_opening(&page, &["1_aaaaaaaa".to_string()]), None);
        assert_eq!(shared_opening(&page, &[]), None);
    }

    #[test]
    fn an_opening_is_trimmed_of_the_joinery_that_led_into_the_difference() {
        // A bare space means the last word is the label introducing the difference — drop it.
        assert_eq!(trim_dangling("A Show, Season 6, Episode "), "A Show, Season 6");
        // Punctuation means the shared part ended there, so the last word belongs to the name.
        assert_eq!(trim_dangling("A Show - "), "A Show");
        // A single word is the whole name, never a label: dropping it would leave nothing.
        assert_eq!(trim_dangling("Entry "), "Entry");
        assert_eq!(trim_dangling("Episode "), "Episode");
        // A trailing word carrying a number is part of the name, not a label for what follows.
        assert_eq!(trim_dangling("Show S1E"), "Show S1E");
        assert_eq!(trim_dangling("Plain"), "Plain");
        assert_eq!(trim_dangling("  -- "), "");
    }

    #[test]
    fn the_opening_is_cut_at_a_character_boundary() {
        // Multi-byte text must not be sliced mid-character just because two strings diverge inside
        // one. `日本` and `日語` share only the first character, not the first byte-and-a-half.
        assert_eq!(common_opening(&["日本語", "日語"]), "日");
        assert_eq!(common_opening(&["abc", "abd"]), "ab");
        assert_eq!(common_opening(&["same", "same"]), "same");
        assert_eq!(common_opening(&["x", "y"]), "");
    }
}
