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
pub(crate) fn sequence(page: &str, ids: &[String]) -> Option<Vec<usize>> {
    let values = described_by(page, ids)?;
    // A name that says which number is the episode is better evidence than any guess, so it is
    // asked first — and it sidesteps the stray entirely rather than detecting it afterwards.
    if let Some(numbered) = marked_sequence(&values) {
        return Some(numbered);
    }
    let numbers: Vec<usize> = values.iter().filter_map(|value| last_number(value)).collect();
    if numbers.len() != values.len() {
        return None;
    }
    let mut sorted = numbers.clone();
    sorted.sort_unstable();
    sorted.windows(2).all(|pair| pair[1] == pair[0] + 1).then_some(numbers)
}

/// Words a name uses to say that the number after it is the episode, longest first so `episode`
/// is recognised before the `e` inside it.
const EPISODE_MARKERS: &[&str] = &["episode", "ep", "e"];

/// The same for a season.
const SEASON_MARKERS: &[&str] = &["season", "se", "s"];

/// The numbers a set of names give *by name* — where each says which of its numbers is the episode.
///
/// This is the trustworthy path. A name reading `Episode 5: The 3 Musketeers` has two numbers in it
/// and only one of them is the episode; anchoring on the word is what tells them apart, where
/// taking the last run of digits would take the stray. So no shape test is imposed on the result
/// beyond the numbers being distinct — they don't have to prove they aren't row ids, because the
/// name already said what they are.
///
/// A season, when *every* name carries one, is folded in ahead of the episode: season 5 episode 4
/// becomes 504, season 5 episode 11 becomes 511. Series with fewer than ten episodes a season leave
/// a nominal zero in there, which costs nothing — sorting by name is the whole purpose, and it still
/// sorts. Used only when every name has one, since mixing 617 with a bare 17 would order them
/// absurdly.
fn marked_sequence(values: &[&str]) -> Option<Vec<usize>> {
    let episodes: Vec<usize> =
        values.iter().filter_map(|value| marked_number(value, EPISODE_MARKERS)).collect();
    if episodes.len() != values.len() {
        return None;
    }
    let seasons: Vec<usize> =
        values.iter().filter_map(|value| marked_number(value, SEASON_MARKERS)).collect();
    let numbers: Vec<usize> = if seasons.len() == values.len() {
        episodes.iter().zip(&seasons).map(|(episode, season)| season * 100 + episode).collect()
    } else {
        episodes
    };
    let distinct: HashSet<&usize> = numbers.iter().collect();
    (distinct.len() == numbers.len()).then_some(numbers)
}

/// The number introduced by the last of `markers` that actually introduces one.
///
/// A marker has to begin a word: the `e` inside `Volume` is not an episode marker, so `Volume 3`
/// yields nothing here and falls to the guesswork instead. Between the marker and its digits any
/// run of space, dot, hyphen or underscore is allowed, or none at all — which is what makes `S3E12`,
/// `Ep. 12` and `episode-12` the same thing.
fn marked_number(text: &str, markers: &[&str]) -> Option<usize> {
    let lower = text.to_lowercase();
    let mut found = None;
    for at in lower.char_indices().map(|(at, _)| at) {
        if lower[..at].chars().next_back().is_some_and(char::is_alphabetic) {
            continue; // mid-word, so not a marker
        }
        for marker in markers {
            if !lower[at..].starts_with(marker) {
                continue;
            }
            // Only a marker that leads to digits counts, so `e` failing inside `episode` doesn't
            // stop the longer word from being tried.
            if let Some(number) = number_after(&lower[at + marker.len()..]) {
                found = Some(number);
                break;
            }
        }
    }
    found
}

/// The number `rest` opens with, past any separator between it and whatever introduced it.
fn number_after(rest: &str) -> Option<usize> {
    let digits = rest.trim_start_matches([' ', '.', '-', '_']);
    let run: String = digits.chars().take_while(|c| c.is_ascii_digit()).collect();
    run.parse().ok()
}

/// The field the page describes these videos by, and its value for each of them in `ids` order.
///
/// One choice serves both the collection's name and its numbering, so the two can't disagree about
/// which field is the descriptive one. Every id must be found: a missing one would shift the values
/// out of step with the videos they belong to, which numbering cannot survive.
fn described_by<'a>(page: &'a str, ids: &[String]) -> Option<Vec<&'a str>> {
    if ids.len() < 2 {
        return None;
    }
    let described: Vec<HashMap<&str, &str>> =
        ids.iter().map(|id| fields_for(page, id)).collect();
    if described.iter().any(HashMap::is_empty) {
        return None;
    }
    // Only fields the page gives *every* video: one missing from a single entry can't describe the
    // set.
    let mut shared: HashSet<&str> = described[0].keys().copied().collect();
    for fields in &described[1..] {
        shared.retain(|key| fields.contains_key(key));
    }
    shared
        .iter()
        .filter_map(|key| {
            let values: Vec<&str> = described.iter().map(|fields| fields[*key]).collect();
            // An id or a category label repeats itself; a title doesn't. This is what tells the
            // descriptive field from a `typeDescription` of "Episode" on every entry.
            let distinct: HashSet<&&str> = values.iter().collect();
            (distinct.len() == values.len()).then_some(values)
        })
        .max_by_key(|values| trim_dangling(common_opening(values)).len())
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

    #[test]
    fn a_collection_is_numbered_by_the_numbers_in_its_own_names() {
        // The real shape: the page lists newest-first, so its order and the true order disagree.
        // The numbers come back in *page* order — they are what each video is called, and the
        // caller pairs them with the sibling it is downloading.
        let ids = ["1_aaaaaaaa", "1_bbbbbbbb", "1_cccccccc"];
        let page = episodes(&["A Show, Episode 3", "A Show, Episode 2", "A Show, Episode 1"], &ids);
        assert_eq!(sequence(&page, &owned(&ids)), Some(vec![3, 2, 1]));

        // A run that doesn't start at one is still a run — the back half of a season, say.
        let later = episodes(&["Ep 12", "Ep 13", "Ep 14"], &ids);
        assert_eq!(sequence(&later, &owned(&ids)), Some(vec![12, 13, 14]));
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
        assert_eq!(marked_number("Episode 5: The 3 Musketeers", EPISODE_MARKERS), Some(5));
        assert_eq!(last_number("Episode 5: The 3 Musketeers"), Some(3), "which is why the word wins");

        // Every spelling, any case, with or without a separator.
        for name in ["Episode 12", "episode-12", "Ep 12", "EP.12", "ep_12", "E12", "e 12"] {
            assert_eq!(marked_number(name, EPISODE_MARKERS), Some(12), "{name}");
        }
        for name in ["Season 5", "season-5", "SE.5", "se_5", "S5", "s 5"] {
            assert_eq!(marked_number(name, SEASON_MARKERS), Some(5), "{name}");
        }
    }

    #[test]
    fn a_marker_has_to_begin_a_word() {
        // The `e` inside an ordinary word is not an episode marker, or every "Volume 3" would be
        // read as an episode. These fall through to the guesswork instead, as they should.
        for name in ["Volume 3", "Phase 2", "Take 5", "Line 9"] {
            assert_eq!(marked_number(name, EPISODE_MARKERS), None, "{name}");
        }
        // And a season marker must not be found inside a word that merely starts with one.
        assert_eq!(marked_number("Series 4", SEASON_MARKERS), None);
        assert_eq!(marked_number("Show 3", SEASON_MARKERS), None);
    }

    #[test]
    fn a_season_is_folded_in_ahead_of_the_episode() {
        let ids = ["1_aaaaaaaa", "1_bbbbbbbb"];
        // The stated shape: season 5 episode 4 is 504, season 5 episode 11 is 511.
        let page = episodes(&["S05E04", "S05E11"], &ids);
        assert_eq!(sequence(&page, &owned(&ids)), Some(vec![504, 511]));
        // Compact and spelled-out forms are the same thing.
        let spelled = episodes(&["Season 3, Episode 12", "Season 3, Episode 2"], &ids);
        assert_eq!(sequence(&spelled, &owned(&ids)), Some(vec![312, 302]));
        // A single-digit season with a nominal zero still sorts, which is the whole point.
        let sorted = {
            let mut got = sequence(&page, &owned(&ids)).unwrap();
            got.sort_unstable();
            got.iter().map(|n| format!("{n:03}")).collect::<Vec<_>>()
        };
        assert_eq!(sorted, ["504", "511"]);

        // A season only counts when every name carries one — mixing 312 with a bare 2 would order
        // them absurdly, so the episode numbers stand alone instead. (The names still share an
        // opening, because that is what identifies them as the descriptive field in the first
        // place; see `described_by`.)
        let mixed = episodes(&["A Show, Season 3, Episode 12", "A Show, Episode 2"], &ids);
        assert_eq!(sequence(&mixed, &owned(&ids)), Some(vec![12, 2]));
    }

    #[test]
    fn marked_numbers_need_not_be_consecutive_because_the_name_vouched_for_them() {
        // The consecutiveness test exists to catch numbers that might not be episode numbers at
        // all. A name saying `Episode` has already settled that, so gaps are allowed — a page
        // listing a selection of episodes is still numbered by them.
        let ids = ["1_aaaaaaaa", "1_bbbbbbbb", "1_cccccccc"];
        let gapped = episodes(&["Episode 1", "Episode 5", "Episode 9"], &ids);
        assert_eq!(sequence(&gapped, &owned(&ids)), Some(vec![1, 5, 9]));
        // A repeat is still refused: two files cannot share one prefix.
        let repeated = episodes(&["Episode 1", "Episode 1 again", "Episode 9"], &ids);
        assert_eq!(sequence(&repeated, &owned(&ids)), None);
    }

    #[test]
    fn numbers_that_are_not_a_sequence_are_refused() {
        let ids = ["1_aaaaaaaa", "1_bbbbbbbb", "1_cccccccc"];
        // One video with no number of its own cannot be numbered by one.
        let missing = episodes(&["Ep 1", "Ep 2", "Trailer"], &ids);
        assert_eq!(sequence(&missing, &owned(&ids)), None);
        // A repeat would put two files on the same prefix.
        let repeated = episodes(&["Ep 1", "Ep 2", "Ep 2 revisited"], &ids);
        assert_eq!(sequence(&repeated, &owned(&ids)), None);
        // Distinct but scattered: these are the page's own row ids, not episode numbers, and this
        // is the case the consecutiveness test exists to reject.
        let internal = episodes(&["Row 2025892", "Row 2025894", "Row 2026744"], &ids);
        assert_eq!(sequence(&internal, &owned(&ids)), None);
        // And one video is not a sequence to find.
        assert_eq!(sequence(&missing, &owned(&["1_aaaaaaaa"])), None);
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
