//! Which cookie domains a download needs.
//!
//! `--cookies-extract-for-domain <target> <dir>` and every download run resolve their argument
//! to a target site: the set of cookie domains worth extracting, and a stable key naming the
//! per-site store dir. An
//! imported store holds ONLY these domains' cookies — never the whole browser DB — so an import
//! for one site cannot leak your banking or email cookies to disk. A site whose auth genuinely
//! spans a second registrable domain (Twitter/X: x.com + legacy twitter.com) lists both.

/// A known site: its `--cookies-extract-for-domain` keyword and download routing, plus the
/// cookie domains to keep.
struct Site {
    /// The keyword (`--cookies-extract-for-domain youtube`) and the per-site store dir name.
    name: &'static str,
    /// Host suffixes that route to this site — a URL or domain whose host equals one of these
    /// (or is a subdomain of it) selects this entry. `youtu.be` routes to `youtube`.
    aliases: &'static [&'static str],
    /// The cookie domains to extract. Usually just the site's own domain; list a second only when
    /// auth genuinely lives there (Twitter/X: `x.com` + legacy `twitter.com`). Deliberately NOT
    /// `google.com` for YouTube — yt-dlp reads YouTube auth from the `youtube.com` jar alone (the
    /// Google-SSO cookies are mirrored there), so adding it would copy the whole Google session to
    /// disk for nothing.
    cookie_domains: &'static [&'static str],
}

/// The curated known sites. `--cookies-extract-for-domain`'s help lists these names (a test
/// keeps the two in sync); anything not here is handled as a bare domain. Order is the help/listing order.
const SITES: &[Site] = &[
    Site { name: "youtube", aliases: &["youtube.com", "youtu.be", "youtube-nocookie.com"], cookie_domains: &["youtube.com"] },
    Site { name: "tiktok", aliases: &["tiktok.com"], cookie_domains: &["tiktok.com"] },
    Site { name: "facebook", aliases: &["facebook.com", "fb.watch"], cookie_domains: &["facebook.com"] },
    Site { name: "instagram", aliases: &["instagram.com"], cookie_domains: &["instagram.com"] },
    Site { name: "twitter", aliases: &["twitter.com", "x.com"], cookie_domains: &["twitter.com", "x.com"] },
    Site { name: "reddit", aliases: &["reddit.com"], cookie_domains: &["reddit.com"] },
    Site { name: "vimeo", aliases: &["vimeo.com"], cookie_domains: &["vimeo.com"] },
    Site { name: "twitch", aliases: &["twitch.tv"], cookie_domains: &["twitch.tv"] },
    Site { name: "niconico", aliases: &["nicovideo.jp", "nico.ms"], cookie_domains: &["nicovideo.jp"] },
    Site { name: "bilibili", aliases: &["bilibili.com", "b23.tv"], cookie_domains: &["bilibili.com"] },
    Site { name: "patreon", aliases: &["patreon.com"], cookie_domains: &["patreon.com"] },
    Site { name: "nebula", aliases: &["nebula.tv", "watchnebula.com"], cookie_domains: &["nebula.tv", "watchnebula.com"] },
    Site { name: "bbc", aliases: &["bbc.co.uk", "bbc.com"], cookie_domains: &["bbc.co.uk", "bbc.com"] },
];

/// Informal keywords that resolve to a curated site whose store key differs from the word people
/// type — the Twitter→X rebrand is the one that matters (`x` → the `twitter` store). Each value is
/// a site `name` in [`SITES`]; a test keeps every one pointing at a real site.
const KEYWORD_ALIASES: &[(&str, &str)] = &[("x", "twitter")];

/// A resolved cookie target: the per-site store dir name (`key`), the cookie domains to keep, and
/// a human `label` for messages. For a known site the key is its curated name; for anything else
/// it's the bare registrable domain, so an unknown site still gets its own isolated store.
#[derive(Debug, PartialEq)]
pub(crate) struct SiteTarget {
    pub key: String,
    pub domains: Vec<String>,
    pub label: String,
}

/// The comma-joined known-site names, for `--cookies-extract-for-domain`'s help text and the
/// sync test.
pub(crate) fn known_site_names() -> String {
    SITES.iter().map(|s| s.name).collect::<Vec<_>>().join(", ")
}

/// Whether `input` is a cookie target we can actually resolve — a known site (keyword, informal
/// alias like `x`, or a URL/domain matching one), or a proper domain/URL whose host carries a dot.
/// A bare word that's neither (e.g. `tiktok2`) maps to no cookie domain, so the extract flag
/// rejects it up front instead of building an empty store.
pub(crate) fn is_importable_target(input: &str) -> bool {
    site_for(input).is_some() || host_of(input.trim()).contains('.')
}

/// The curated [`Site`] an input names — a keyword by `name` (or an informal alias like `x` →
/// twitter), or a URL/domain by its host matching an alias. `None` for anything not curated. This
/// is the shared "which known site is this?" recognition: [`resolve_target`] and
/// [`is_importable_target`] use it today, and it's the seam a future caller can reuse to fold a
/// site's many addresses back to the one it owns.
fn site_for(input: &str) -> Option<&'static Site> {
    let input = input.trim();
    // A keyword matches by `name`, after mapping any informal alias (x → twitter) onto it.
    let canonical = KEYWORD_ALIASES
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(input))
        .map_or(input, |(_, name)| name);
    if let Some(site) = SITES.iter().find(|s| s.name.eq_ignore_ascii_case(canonical)) {
        return Some(site);
    }
    // Otherwise treat it as a URL/domain and match its host against the sites' aliases.
    SITES.iter().find(|s| host_matches_any(&host_of(input), s.aliases))
}

/// Resolve a `--cookies-extract-for-domain` argument — a site keyword (`youtube`), a domain
/// (`tiktok.com`), or a full URL (`https://www.tiktok.com/@u/video/1`) — to its [`SiteTarget`]. Also used per
/// download run to find which imported store (if any) serves the download's host.
pub(crate) fn resolve_target(input: &str) -> SiteTarget {
    if let Some(site) = site_for(input) {
        return site.target();
    }
    // Unknown → its own registrable domain, isolated store, only that domain's cookies.
    let domain = registrable_domain(&host_of(input.trim()));
    SiteTarget { key: domain.clone(), domains: vec![domain.clone()], label: domain }
}

impl Site {
    fn target(&self) -> SiteTarget {
        SiteTarget {
            key: self.name.to_string(),
            domains: self.cookie_domains.iter().map(|d| d.to_string()).collect(),
            label: self.name.to_string(),
        }
    }
}

/// True when `host` equals one of `domains` or is a subdomain of it (dot-boundary match, so
/// `evil-youtube.com` and `youtube.com.evil.test` don't match `youtube.com`).
fn host_matches_any(host: &str, domains: &[&str]) -> bool {
    domains.iter().any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// The lowercased host of a URL or bare domain — scheme, userinfo, port, path, and a leading
/// `www.` stripped. A value with no host (a bare path) yields `""`.
pub(crate) fn host_of(input: &str) -> String {
    let after_scheme = input.split_once("://").map_or(input, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority); // drop any user:pass@
    let host = host.split(':').next().unwrap_or(host).to_lowercase(); // drop any :port
    host.strip_prefix("www.").unwrap_or(&host).to_string()
}

/// The registrable domain of a host by the simple last-two-labels rule (`m.example.com` →
/// `example.com`). Good for the common single-part TLDs; a multi-part TLD (`bbc.co.uk`) resolves
/// one label short (`co.uk`) — acceptable, since known sites match by alias before reaching here
/// and the fallback only scopes an unknown site's own store.
fn registrable_domain(host: &str) -> String {
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    match labels.len() {
        0 => String::new(),
        n => labels[n.saturating_sub(2)..].join("."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_target_maps_keywords_domains_and_urls_to_sites() {
        // A keyword, a domain, and a URL all resolve to the youtube site — keeping only youtube.com
        // cookies (never google.com: yt-dlp reads YouTube auth from the youtube.com jar alone, so
        // pulling google.com would copy the user's whole Google session to disk for nothing).
        for input in ["youtube", "youtube.com", "https://www.youtube.com/watch?v=x", "https://youtu.be/x"] {
            let t = resolve_target(input);
            assert_eq!(t.key, "youtube", "{input}");
            assert_eq!(t.domains, ["youtube.com"], "youtube keeps only its own domain: {input}");
        }
        // A URL to a known site isolates by keyword; twitter aliases x.com.
        assert_eq!(resolve_target("https://x.com/i/status/1").key, "twitter");
        // An unknown site → its own registrable domain, only that domain.
        let unknown = resolve_target("https://videos.example.co/watch/9");
        assert_eq!(unknown.key, "example.co");
        assert_eq!(unknown.domains, ["example.co"]);
    }

    #[test]
    fn added_sites_resolve_by_keyword_and_url_including_dual_domain_ones() {
        // Single-domain additions: the keyword lands on the site's own domain, a URL routes by alias.
        assert_eq!(resolve_target("bilibili").domains, ["bilibili.com"]);
        assert_eq!(resolve_target("https://www.bilibili.com/video/BV1").key, "bilibili");
        assert_eq!(resolve_target("niconico").domains, ["nicovideo.jp"]);
        assert_eq!(resolve_target("patreon").domains, ["patreon.com"]);
        // Nebula keeps its current and legacy domains.
        assert_eq!(resolve_target("nebula").domains, ["nebula.tv", "watchnebula.com"]);
        // BBC spans two registrable domains; a bbc.co.uk URL matches by alias, so it keeps BOTH —
        // and never falls back to the last-two-labels rule, which would wrongly yield just `co.uk`.
        let bbc = resolve_target("https://www.bbc.co.uk/iplayer/episode/x");
        assert_eq!(bbc.key, "bbc");
        assert_eq!(bbc.domains, ["bbc.co.uk", "bbc.com"]);
        // Every added keyword is importable.
        for kw in ["niconico", "bilibili", "patreon", "nebula", "bbc"] {
            assert!(is_importable_target(kw), "{kw}");
        }
    }

    #[test]
    fn importable_targets_are_keywords_or_dotted_hosts_not_bare_words() {
        assert!(is_importable_target("tiktok")); // known keyword
        assert!(is_importable_target("example.com")); // bare domain
        assert!(is_importable_target("https://www.tiktok.com/@u/video/1")); // URL
        assert!(!is_importable_target("tiktok2")); // a bare word, not a keyword, no dot
        assert!(!is_importable_target("com")); // not a domain on its own
    }

    #[test]
    fn host_of_strips_scheme_userinfo_port_path_and_www() {
        assert_eq!(host_of("https://www.youtube.com/watch?v=x"), "youtube.com");
        assert_eq!(host_of("http://user:pass@Host.EXAMPLE.com:8080/a/b"), "host.example.com");
        assert_eq!(host_of("youtu.be/abc"), "youtu.be"); // scheme-less
        assert_eq!(host_of("tiktok.com"), "tiktok.com"); // bare domain
        assert_eq!(host_of("/local/path.mp4"), ""); // no host
    }

    #[test]
    fn known_site_names_stay_in_sync_with_the_help_text() {
        // `--cookies-extract-for-domain`'s help hardcodes this list; if a site is added/removed here, update
        // the arg's help too (this guard makes the drift a test failure, not a silent mismatch).
        assert_eq!(
            known_site_names(),
            "youtube, tiktok, facebook, instagram, twitter, reddit, vimeo, twitch, niconico, bilibili, patreon, nebula, bbc"
        );
    }

    #[test]
    fn keyword_aliases_resolve_to_real_sites_and_x_means_twitter() {
        for (alias, name) in KEYWORD_ALIASES {
            assert!(SITES.iter().any(|s| s.name == *name), "alias {alias} points at unknown site {name}");
        }
        assert_eq!(resolve_target("x").key, "twitter"); // the Twitter→X rebrand keyword
        assert!(is_importable_target("x"), "x resolves via the twitter alias, so it's importable");
    }

    /// A site key becomes a directory name under the cookie root, so a key that is not a plain
    /// path segment would escape it — `..`, a slash, or an empty string turning `<root>/<key>`
    /// into somewhere else entirely. Curated names are safe by inspection; this pins the rule for
    /// whoever adds the fourteenth.
    #[test]
    fn every_site_key_is_a_safe_directory_name() {
        for site in SITES {
            let key = resolve_target(site.name).key;
            assert!(!key.is_empty(), "{}: empty key", site.name);
            assert!(!key.contains('/') && key != ".." && key != ".", "{}: `{key}` escapes the root", site.name);
            assert!(!site.cookie_domains.is_empty(), "{}: a site with no domains keeps nothing", site.name);
            assert!(
                site.aliases.iter().any(|alias| host_of(alias) == *alias),
                "{}: an alias must already be a bare host",
                site.name
            );
        }
    }
}
