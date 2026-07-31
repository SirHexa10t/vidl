//! Compute each video's exact subtitle-track list from YouTube's ~157-language caption
//! matrix — batch-probing entries and grouping them by language keys, so every video gets
//! the uploader's own languages plus English without downloading the whole matrix.

use std::ffi::OsString;

use crate::util::exec::capture_stdout;
use crate::Env;
use crate::ytdlp::{seeded, ytdlp_invocation};

/// One subtitle track the plan requests: its yt-dlp language key, YouTube's display name for
/// it (which becomes the embedded track title — what the post-pass matches on), and whether
/// it's auto-generated rather than the uploader's.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Pick {
    pub(crate) key: String,
    pub(crate) name: String,
    pub(crate) auto: bool,
}

/// The subtitle tracks to embed for one video, given what exists. The policy: every language
/// the uploader published; the video's native language as auto-captions when the uploader
/// published none in it (the raw `-orig` track, else the plain one); and EN — real, else
/// auto-translated (the `en-<lang>` pair when published, plain `en` otherwise) — unless a real
/// or native EN already covers it. Nothing is ever doubled, and every auto pick is flagged so
/// [`mark_auto_titles`] can stamp its embedded title.
///
/// Every pick is a MUST-TRY, never a must-have — EN above all: YouTube lists auto-translations
/// for every video but its translation endpoint is aggressively rate-limited (429s, sometimes
/// permanently on shared/VPN IPs), so a listed track may never be served. `--ignore-errors`
/// rides every download exactly so a refused subtitle downgrades to a warning and the video
/// completes without it — don't "fix" a sub-only failure into a failed exit.
pub(crate) fn sub_picks_for(
    reals: &[(String, String)],
    autos: &[(String, String)],
    orig: Option<&str>,
) -> Vec<Pick> {
    let base = |lang: &str| lang.split('-').next().unwrap_or(lang).to_lowercase();
    let auto_pick = |key: &str| {
        autos.iter().find(|(auto_key, _)| auto_key == key).map(|(auto_key, name)| Pick {
            key: auto_key.clone(),
            name: if name.is_empty() { auto_key.clone() } else { name.clone() },
            auto: true,
        })
    };
    let mut picks: Vec<Pick> = reals
        .iter()
        .filter(|(key, _)| key != "live_chat")
        .map(|(key, name)| Pick { key: key.clone(), name: name.clone(), auto: false })
        .collect();
    let native = orig.filter(|orig| !orig.is_empty()).map(&base);
    if let Some(native) = &native {
        if !picks.iter().any(|pick| base(&pick.key) == *native) {
            if let Some(pick) = auto_pick(&format!("{native}-orig")).or_else(|| auto_pick(native))
            {
                picks.push(pick);
            }
        }
    }
    if !picks.iter().any(|pick| base(&pick.key) == "en") {
        let translated = native.as_ref().and_then(|native| auto_pick(&format!("en-{native}")));
        if let Some(pick) = translated.or_else(|| auto_pick("en")) {
            picks.push(pick);
        }
    }
    picks
}

/// The pre-probe fallback (and the no-probe path): the EN-family behavior — not flagged auto,
/// since without a probe nobody knows, and a wrong "(auto-generated)" stamp is worse than none.
pub(crate) fn default_picks() -> Vec<Pick> {
    ["en", "en-US", "en-GB"]
        .map(|key| Pick { key: key.to_string(), name: String::new(), auto: false })
        .to_vec()
}

/// One video's subtitle plan plus its id (the post-pass finds the file by it). An unprobeable
/// video falls back to the EN default — resilience over completeness.
pub(crate) fn video_picks(url: &str, env: Env) -> (Option<String>, Vec<Pick>) {
    let mut argv = seeded(env);
    argv.extend(
        [
            "--print", "%(id)s",
            "--print", "%(language)s",
            "--print", "%(subtitles)j",
            "--print", "%(automatic_captions)j",
        ]
        .map(OsString::from),
    );
    argv.push(url.into());
    let (program, args) = ytdlp_invocation(argv);
    let Some(out) = capture_stdout(program, args) else {
        return (None, default_picks());
    };
    let mut lines = out.lines();
    let id = lines.next().map(str::trim).filter(|id| !id.is_empty() && *id != "NA");
    let (Some(language), Some(subs), Some(autos)) = (lines.next(), lines.next(), lines.next())
    else {
        return (id.map(str::to_string), default_picks());
    };
    let orig = Some(language.trim()).filter(|lang| !lang.is_empty() && *lang != "NA");
    let picks = sub_picks_for(&json_lang_names(subs), &json_lang_names(autos), orig);
    (id.map(str::to_string), picks)
}

/// Per-entry print format of the batch subtitle probe.
const PROBE_FORMAT: &str =
    "%(playlist_index)s\t%(id)s\t%(language)s\t%(subtitles)j\t%(automatic_captions)j";

/// Probes larger than this pace themselves with `--sleep-requests` (small ones finish before
/// any rate-limiter would care).
const PROBE_PACING_THRESHOLD: usize = 20;

/// One pending entry with its computed subtitle plan.
pub(crate) struct Planned {
    pub(crate) index: String,
    pub(crate) id: String,
    pub(crate) picks: Vec<Pick>,
}

/// Probe the subtitle situation of many entries in ONE yt-dlp invocation (process startup and
/// player work are the expensive parts — per-entry probes multiply them). Entries that fail to
/// extract are simply absent; callers fall back to [`default_picks`] for those.
pub(crate) fn batch_probe(url: &str, indexes: &[String], env: Env) -> Vec<Planned> {
    let mut argv = seeded(env);
    argv.push("--ignore-errors".into());
    if indexes.len() > PROBE_PACING_THRESHOLD {
        // A big probe is a burst of metadata requests with no downloads between them to slow
        // the rate; pace it below YouTube's radar (~24/min measured) rather than risk the IP
        // getting flagged before the first byte of video downloads.
        argv.extend(["--sleep-requests", "1"].map(OsString::from));
    }
    argv.extend([
        OsString::from("--playlist-items"), indexes.join(",").into(),
        "--print".into(), PROBE_FORMAT.into(),
        url.into(),
    ]);
    let (program, args) = ytdlp_invocation(argv);
    capture_stdout(program, args).map(|out| parse_batch(&out)).unwrap_or_default()
}

/// Parse [`batch_probe`]'s lines into per-entry plans.
fn parse_batch(out: &str) -> Vec<Planned> {
    out.lines()
        .filter_map(|line| {
            let mut fields = line.splitn(5, '\t');
            let (index, id, language, subs, autos) =
                (fields.next()?, fields.next()?, fields.next()?, fields.next()?, fields.next()?);
            let orig = (!language.is_empty() && language != "NA").then_some(language);
            Some(Planned {
                index: index.to_string(),
                id: id.to_string(),
                picks: sub_picks_for(&json_lang_names(subs), &json_lang_names(autos), orig),
            })
        })
        .collect()
}

/// Group planned entries by their subtitle KEY list, so each distinct list becomes ONE download
/// invocation (`--playlist-items` takes a comma list) instead of one per entry. Auto flags may
/// differ within a group (the same `en` can be real for one entry, translated for another) —
/// that's per-entry data the post-pass reads from each [`Planned`].
pub(crate) fn group_by_langs(planned: &[Planned]) -> Vec<Vec<&Planned>> {
    let keys = |plan: &Planned| plan.picks.iter().map(|pick| pick.key.clone()).collect::<Vec<_>>();
    let mut groups: Vec<(Vec<String>, Vec<&Planned>)> = Vec::new();
    for plan in planned {
        let plan_keys = keys(plan);
        match groups.iter_mut().find(|(group_keys, _)| *group_keys == plan_keys) {
            Some((_, members)) => members.push(plan),
            None => groups.push((plan_keys, vec![plan])),
        }
    }
    groups.into_iter().map(|(_, members)| members).collect()
}

/// The top-level keys of a JSON object paired with the first `"name"` string inside each
/// key's value — dependency-free, same walking technique as [`json_top_keys`]. Fits yt-dlp's
/// `%(subtitles)j` shape: `{"en": [{"url": …, "name": "English"}, …], …}`; a key whose value
/// carries no name gets `""`.
fn json_lang_names(json: &str) -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut depth = 0u32;
    let mut in_string: Option<String> = None;
    let mut escaped = false;
    let mut pending: Option<(u32, String)> = None; // a closed string, with its depth
    let mut await_name_value = false; // the last string was a `"name"` key inside a value
    for ch in json.chars() {
        if let Some(buffer) = in_string.as_mut() {
            if escaped {
                buffer.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                let text = in_string.take().unwrap();
                if await_name_value {
                    await_name_value = false;
                    if let Some((_, name)) = entries.last_mut() {
                        if name.is_empty() {
                            *name = text;
                        }
                    }
                } else {
                    pending = Some((depth, text));
                }
            } else {
                buffer.push(ch);
            }
            continue;
        }
        match ch {
            '"' => in_string = Some(String::new()),
            ':' => match pending.take() {
                Some((1, key)) => entries.push((key, String::new())),
                Some((_, key)) if key == "name" => await_name_value = true,
                _ => {}
            },
            ' ' | '\t' | '\n' | '\r' => {}
            '{' | '[' => {
                depth += 1;
                pending = None;
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                pending = None;
            }
            _ => {
                pending = None;
                await_name_value = false;
            }
        }
    }
    entries
}


#[cfg(test)]
mod tests {
    use super::*;

    /// `(key, name)` pairs the way the probe parser hands them over.
    fn named(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items.iter().map(|(key, name)| (key.to_string(), name.to_string())).collect()
    }

    fn keys_of(picks: &[Pick]) -> Vec<&str> {
        picks.iter().map(|pick| pick.key.as_str()).collect()
    }

    #[test]
    fn subtitle_policy_covers_uploader_native_and_english_without_doubles() {
        // The PlayStation case: uploader published en+ko — both taken, no auto anything.
        let real = sub_picks_for(
            &named(&[("en", "English"), ("ko", "Korean")]),
            &named(&[("ko", "Korean"), ("ko-orig", "Korean (Original)"), ("en", "English")]),
            Some("ko"),
        );
        assert_eq!(keys_of(&real), ["en", "ko"]);
        assert!(real.iter().all(|pick| !pick.auto), "uploader subs must never be stamped");

        // No uploader subs on a Korean video: native auto + translated EN, both flagged auto,
        // preferring the pair/orig keys when published.
        let auto = sub_picks_for(
            &[],
            &named(&[
                ("ko", "Korean"),
                ("ko-orig", "Korean (Original)"),
                ("en", "English"),
                ("en-ko", "English from Korean"),
            ]),
            Some("ko"),
        );
        assert_eq!(keys_of(&auto), ["ko-orig", "en-ko"]);
        assert!(auto.iter().all(|pick| pick.auto));
        assert_eq!(auto[0].name, "Korean (Original)", "the name is what the post-pass matches");

        // The Hebrew case, legacy `iw` code and all — no pair key published, plain `en` plan B.
        let hebrew = sub_picks_for(
            &[],
            &named(&[("iw", "Hebrew"), ("iw-orig", "Hebrew (Original)"), ("en", "English")]),
            Some("iw"),
        );
        assert_eq!(keys_of(&hebrew), ["iw-orig", "en"]);
        assert!(hebrew.iter().all(|pick| pick.auto), "plain `en` here is still a translation");

        // English-native video with no uploader subs: EN once, not twice.
        assert_eq!(
            keys_of(&sub_picks_for(
                &[],
                &named(&[("en", "English"), ("en-orig", "English (Original)")]),
                Some("en")
            )),
            ["en-orig"]
        );
        // Real Korean only: EN still arrives as the auto translation, flagged as such.
        let mixed = sub_picks_for(
            &named(&[("ko", "Korean")]),
            &named(&[("en", "English"), ("ko-orig", "Korean (Original)")]),
            Some("ko"),
        );
        assert_eq!(keys_of(&mixed), ["ko", "en"]);
        assert_eq!(mixed.iter().map(|pick| pick.auto).collect::<Vec<_>>(), [false, true]);
        // live_chat is not a subtitle; unknown native language degrades gracefully.
        assert_eq!(
            keys_of(&sub_picks_for(&named(&[("live_chat", ""), ("en", "English")]), &[], None)),
            ["en"]
        );
        assert!(sub_picks_for(&[], &[], None).is_empty(), "nothing available, nothing requested");
    }

    #[test]
    fn json_lang_names_pair_each_key_with_its_display_name() {
        let json = r#"{"en": [{"url": "u", "name": "English"}], "iw-orig": [{"ext": "vtt", "name": "Hebrew (Original)"}], "bare": []}"#;
        assert_eq!(
            json_lang_names(json),
            [
                ("en".to_string(), "English".to_string()),
                ("iw-orig".to_string(), "Hebrew (Original)".to_string()),
                ("bare".to_string(), String::new()),
            ]
        );
        assert!(json_lang_names("NA").is_empty());
    }

    #[test]
    fn the_batch_probe_parses_per_entry_and_groups_by_subtitle_keys() {
        // Entry 1: real en+ko. Entry 2: nothing anywhere. Entry 3: auto-only Korean video.
        let out = "1\tidA00000001\tko\t{\"en\": [{\"name\": \"English\"}], \"ko\": [{\"name\": \"Korean\"}]}\t{}\n\
                   2\tidB00000002\tNA\t{}\t{}\n\
                   3\tidC00000003\tko\t{}\t{\"ko\": [{\"name\": \"Korean\"}], \"en\": [{\"name\": \"English\"}]}\n";
        let plan = parse_batch(out);
        assert_eq!(plan[0].id, "idA00000001");
        assert_eq!(keys_of(&plan[0].picks), ["en", "ko"]);
        assert!(plan[1].picks.is_empty());
        assert_eq!(keys_of(&plan[2].picks), ["ko", "en"]);
        assert!(plan[2].picks.iter().all(|pick| pick.auto));
        let groups = group_by_langs(&plan);
        assert_eq!(groups.len(), 3, "three distinct key lists here");
        // Same keys → one group, even when the auto flags differ per entry (real en for one,
        // translated en for another) — the post-pass reads each entry's own picks.
        let twin = [
            Planned { index: "1".into(), id: "a".into(), picks: plan[0].picks.clone() },
            Planned {
                index: "9".into(),
                id: "b".into(),
                picks: plan[0]
                    .picks
                    .iter()
                    .map(|pick| Pick { auto: !pick.auto, ..pick.clone() })
                    .collect(),
            },
        ];
        let merged = group_by_langs(&twin);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].iter().map(|plan| plan.index.as_str()).collect::<Vec<_>>(), ["1", "9"]);
    }
}
