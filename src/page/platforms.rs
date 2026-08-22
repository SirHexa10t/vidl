//! How to address a video that a page names by bare id.
//!
//! [`super`] finds sibling ids on a page; this turns one into something yt-dlp accepts. That step
//! is the only part of the feature that cannot be derived from the page, because there is no
//! general id-to-URL function — yt-dlp has some 1800 extractors and each addresses its media its
//! own way. So this is a table, and it is the honest cost of the feature.
//!
//! It is a table of **embed platforms**, not of sites. One Kaltura entry covers every broadcaster
//! that embeds Kaltura, which is a large slice of them — the broadcaster page that prompted this is
//! one of thousands the same rule serves. A site-specific table would be a different thing, and
//! worse.
//!
//! Adding a platform means answering two questions about it: what do its ids look like, and what
//! URL reaches one. Both must be answerable from the page text alone.
//!
//! # Why there is only one entry
//!
//! Not for want of trying. The sibling sweep in [`super`] finds videos by *id shape*, so a platform
//! is only a candidate if its ids are shaped distinctively enough to pick out of a page of text.
//! Kaltura's are: a digit, an underscore, then eight to twelve lowercase alphanumerics. The two
//! obvious next platforms are not, and that is a property of them rather than of this code:
//!
//! - **Brightcove** ids are plain digits (`4463358922011`), which would match every long number a
//!   page happens to contain. Its URL form also needs *three* discovered values — account, player
//!   and embed — where Kaltura needs one.
//! - **JW Player** ids are eight alphanumerics with no separator (`nPripu9l`), which matches a
//!   great many ordinary tokens.
//!
//! So the useful test for a new entry is not "is the platform popular" but "could its ids be told
//! apart from prose". A platform failing that would need the sweep to find its videos some other
//! way — through URLs on the page, say — which is a different feature.

/// An embed platform whose media a page can name by bare id.
pub(crate) struct Platform {
    /// What to call it in a message.
    pub(crate) name: &'static str,
    /// Whether `id` has this platform's id shape. Used twice: once to work out which platform the
    /// page's *resolved* video belongs to, and then to find its siblings by the same shape.
    pub(crate) claims: fn(&str) -> bool,
    /// The URL form yt-dlp accepts for `id`, reading whatever account-level identifier the scheme
    /// needs out of `page`. `None` when the page doesn't carry it — in which case the siblings are
    /// found but not reachable, and [`super`] says so rather than guessing.
    pub(crate) address: fn(&str, &str) -> Option<String>,
}

/// Every platform this crate can address a bare id on.
pub(crate) const PLATFORMS: &[Platform] =
    &[Platform { name: "Kaltura", claims: kaltura_claims, address: kaltura_address }];

/// The platform whose id shape `id` fits, if any.
pub(crate) fn claiming(id: &str) -> Option<&'static Platform> {
    PLATFORMS.iter().find(|platform| (platform.claims)(id))
}

/// The platform names, for the notice that says what `--multi` can and can't reach.
pub(crate) fn names() -> Vec<&'static str> {
    PLATFORMS.iter().map(|platform| platform.name).collect()
}

/// A Kaltura entry id: a single digit, an underscore, then eight to twelve lowercase alphanumerics
/// (`1_8jx8mama`). Deliberately narrow — this shape is also what finds the siblings, so a loose
/// pattern would sweep in every other underscore-joined token on the page.
fn kaltura_claims(id: &str) -> bool {
    let mut chars = id.chars();
    let digit = chars.next().is_some_and(|c| c.is_ascii_digit());
    let underscore = chars.next() == Some('_');
    let rest: Vec<char> = chars.collect();
    digit
        && underscore
        && (8..=12).contains(&rest.len())
        && rest.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// `kaltura:<partner>:<entry>`, which yt-dlp's Kaltura extractor takes directly.
///
/// The partner id is read off the page, so no account knowledge is baked in here — but it must be
/// read off a **kaltura.com** URL specifically, and that detail is the whole difficulty. The page
/// this was built against carries two partner ids: its own, which it declares as `partner_id` and
/// repeats 34 times in its image paths, and the one the media actually lives under, which appears
/// exactly once, in the player script's URL. Only the second addresses the entries — the first is
/// rejected with `Invalid entry id`. So a plain `/p/<digits>` scan finds the wrong one, and being
/// declared by the page is no recommendation either.
///
/// Requiring the host means a Kaltura behind a custom CNAME won't be found. That reports itself as
/// an unreachable sibling rather than guessing an account, which is the right way round.
fn kaltura_address(id: &str, page: &str) -> Option<String> {
    let partner = digits_after(page, "kaltura.com/p/")?;
    Some(format!("kaltura:{partner}:{id}"))
}

/// The run of digits immediately following the first `marker` in `text`, if it is non-empty.
/// A hand-rolled scan rather than a regex: one dependency-free helper against a crate that has
/// two dependencies on purpose.
fn digits_after(text: &str, marker: &str) -> Option<String> {
    let mut from = 0;
    while let Some(at) = text[from..].find(marker) {
        let start = from + at + marker.len();
        let digits: String =
            text[start..].chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() {
            return Some(digits);
        }
        from = start;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kaltura_entry_id_is_recognised_and_a_look_alike_token_is_not() {
        // The real shape, from the page that prompted this.
        assert!(kaltura_claims("1_8jx8mama"));
        assert!(kaltura_claims("0_abcd1234"));
        assert!(kaltura_claims("1_abcdefghijkl"), "twelve trailing characters is still one");
        // The pattern doubles as the sibling finder, so everything it must NOT sweep up off a
        // page: bare numbers, ids without the digit prefix, mixed case, wrong length.
        for not_an_id in [
            "274874100",     // the sub-partner id, all digits
            "sp_274874100",  // an underscore-joined token that doesn't start with a digit
            "1_8JX8MAMA",    // upper case
            "1_short",       // too few trailing characters
            "1_abcdefghijklm", // too many
            "1_",
            "",
            "entryId",
        ] {
            assert!(!kaltura_claims(not_an_id), "{not_an_id} must not be taken for an entry id");
        }
    }

    #[test]
    fn the_partner_id_is_taken_from_a_kaltura_host_and_not_from_the_page_itself() {
        // The shape it appears in on a real page: the player script's own URL.
        let page = r#"<script src="https://cdnapisec.kaltura.com/p/2748741/embedPlaykitJs/x">"#;
        assert_eq!(
            kaltura_address("1_kmayywon", page).as_deref(),
            Some("kaltura:2748741:1_kmayywon")
        );

        // The bug this cost: the page that prompted the feature declares a *different* partner id
        // as its own and repeats it in every image path, and that one is rejected by Kaltura. The
        // count is no guide and neither is the declaration — only the host is.
        let two = r#"{"partner_id":"5031"} <img src="https://cdn.site.test/image/p/5031/a.jpg">
                     <img src="https://cdn.site.test/image/p/5031/b.jpg">
                     <script src="https://cdnapisec.kaltura.com/p/2748741/embedPlaykitJs/x">"#;
        assert_eq!(
            kaltura_address("1_x", two).as_deref(),
            Some("kaltura:2748741:1_x"),
            "the site's own account must not win on frequency or on being declared"
        );

        // Nothing on a Kaltura host means nothing is claimed: the siblings are found but reported
        // unreachable rather than addressed to a guessed account.
        assert!(kaltura_address("1_x", r#"{"partner_id":"5031"} /p/5031/a"#).is_none());
        assert!(kaltura_address("1_x", "no partner anywhere").is_none());
        // A marker with no digits after it must not be mistaken for one; the scan keeps looking.
        assert_eq!(
            kaltura_address("1_x", "kaltura.com/p/x then kaltura.com/p/99/").as_deref(),
            Some("kaltura:99:1_x")
        );
    }

    #[test]
    fn the_table_is_well_formed_and_its_shapes_do_not_overlap() {
        assert!(!PLATFORMS.is_empty());
        assert_eq!(names(), ["Kaltura"], "the notice names exactly what is reachable");
        for platform in PLATFORMS {
            assert!(!platform.name.is_empty());
        }
        // `claiming` must be unambiguous: two platforms claiming one id would make the sibling
        // sweep depend on table order.
        let ids = ["1_8jx8mama", "0_abcd1234"];
        for id in ids {
            let claims = PLATFORMS.iter().filter(|p| (p.claims)(id)).count();
            assert!(claims <= 1, "{id} is claimed by {claims} platforms");
        }
        assert_eq!(claiming("1_8jx8mama").map(|p| p.name), Some("Kaltura"));
        assert!(claiming("dQw4w9WgXcQ").is_none(), "a YouTube id is nobody's business here");
    }
}
