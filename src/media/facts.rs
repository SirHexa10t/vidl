//! What YouTube says about a video *now*, for a repair to hold a downloaded file against: the
//! caption tracks it offers, which the subtitle plan picks from ([`super::subtitles`]); the
//! metadata a download embeds; and its chapters.
//!
//! Asked of yt-dlp in parts of [`ASKED_AT_ONCE`] videos — few processes for a library of hundreds,
//! yet each part said as it is answered, as a run's answers come all at once and a large library,
//! paced like a playlist probe past the size YouTube notices, takes a second a video. A video
//! yt-dlp cannot answer for (removed, bot-walled, the network down) is no reason to drop the
//! others' answers: each comes back with the reason yt-dlp gave.

use std::ffi::OsString;

use crate::Env;
use crate::util::exec::capture_output;
use crate::util::json::{self, Json};
use crate::ytdlp::{seeded, ytdlp_invocation};

use super::subtitles::{json_lang_names, sub_picks_for, Pick, PROBE_PACING_THRESHOLD};

/// One video, as YouTube describes it now.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Facts {
    pub(crate) id: String,
    /// The subtitle tracks a download of it would embed ([`sub_picks_for`]).
    pub(crate) picks: Vec<Pick>,
    /// The metadata a download embeds, under the keys yt-dlp hands ffmpeg (`-metadata title=…`) —
    /// only those YouTube has a value for.
    pub(crate) tags: Vec<(&'static str, String)>,
    pub(crate) chapters: Vec<Chapter>,
}

/// One chapter: where it starts and ends, in seconds, and what it is called.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Chapter {
    pub(crate) start: f64,
    pub(crate) end: f64,
    pub(crate) title: String,
}

/// What a run learned: the videos YouTube answered for, and for each it did not, why.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Answers {
    pub(crate) facts: Vec<Facts>,
    pub(crate) unanswered: Vec<(String, String)>,
}

/// One tab-separated line per video. Every field that can hold a tab or a line break is JSON
/// (`+j`: with its unicode as it is), and each metadata field names the same fields, in the same
/// order of preference, as yt-dlp's `--embed-metadata` does — `track` before `title`, a music
/// video's `artist` before the uploader — so a completed file holds what a download would.
const FACTS_FORMAT: &str = "%(id)s\t%(language)s\t%(subtitles)j\t%(automatic_captions)j\t\
                            %(track,title)+j\t%(upload_date)s\t%(description)+j\t%(webpage_url)s\t\
                            %(artist,artists,creator,creators,uploader,uploader_id)+j\t\
                            %(genre,genres)+j\t%(album)+j\t%(chapters)+j";

/// How many videos one yt-dlp run is asked about.
const ASKED_AT_ONCE: usize = 50;

/// The page a YouTube video is watched at — which yt-dlp is asked about it by.
pub(crate) fn watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

/// Ask YouTube about every video in `ids`: in parts of [`ASKED_AT_ONCE`], each counted off as it
/// is answered when there is more than one.
pub(crate) fn ask_youtube(ids: &[String], env: Env) -> Answers {
    let paced = ids.len() > PROBE_PACING_THRESHOLD;
    let mut answers = Answers::default();
    let mut asked = 0;
    for part in ids.chunks(ASKED_AT_ONCE) {
        let answered = ask_once(part, env, paced);
        answers.facts.extend(answered.facts);
        answers.unanswered.extend(answered.unanswered);
        asked += part.len();
        if ids.len() > ASKED_AT_ONCE {
            println!("  asked about {asked} of {}", ids.len());
        }
    }
    answers
}

/// One yt-dlp run's worth of [`ask_youtube`].
fn ask_once(ids: &[String], env: Env, paced: bool) -> Answers {
    let mut argv = seeded(env);
    argv.push("--ignore-errors".into());
    if paced {
        argv.extend(["--sleep-requests", "1"].map(OsString::from));
    }
    argv.extend(["--print", FACTS_FORMAT].map(OsString::from));
    argv.extend(ids.iter().map(|id| OsString::from(watch_url(id))));
    let (program, args) = ytdlp_invocation(argv);
    match capture_output(program, args) {
        // Read whatever came back whatever the exit status: with `--ignore-errors`, one video
        // refused still exits 1, after printing every other video's answer.
        Some((_, out, err)) => read_answers(ids, &out, &err),
        None => Answers {
            facts: Vec::new(),
            unanswered: ids.iter().map(|id| (id.clone(), "yt-dlp could not be run".to_string())).collect(),
        },
    }
}

/// [`ask_youtube`]'s output read back: an answer per [`FACTS_FORMAT`] line, and every video asked
/// about without one, with the reason yt-dlp gave on `err` — its `ERROR: [youtube] <id>: …` line,
/// or else the run's last error.
fn read_answers(ids: &[String], out: &str, err: &str) -> Answers {
    let facts: Vec<Facts> = out.lines().filter_map(read_facts).collect();
    let last_error = err.lines().rev().find_map(|line| line.strip_prefix("ERROR: "));
    let unanswered = ids
        .iter()
        .filter(|id| !facts.iter().any(|facts| facts.id == **id))
        .map(|id| {
            let marker = format!("] {id}: ");
            let own = err.lines().find_map(|line| line.split_once(&marker).map(|(_, why)| why));
            let why = own.or(last_error).unwrap_or("YouTube gave no answer");
            (id.clone(), why.trim().to_string())
        })
        .collect();
    Answers { facts, unanswered }
}

/// One [`FACTS_FORMAT`] line; `None` for anything else yt-dlp printed.
fn read_facts(line: &str) -> Option<Facts> {
    let fields: Vec<&str> = line.split('\t').collect();
    let [id, language, subtitles, automatic, title, date, description, url, artist, genre, album, chapters] =
        fields[..]
    else {
        return None;
    };
    let given = |field: &str| (!field.is_empty() && field != "NA").then(|| field.to_string());
    let id = given(id)?;
    let picks = sub_picks_for(&json_lang_names(subtitles), &json_lang_names(automatic), given(language).as_deref());
    let description = text(description);
    let url = given(url);
    let tags: Vec<(&'static str, String)> = [
        ("title", text(title)),
        ("date", given(date)),
        ("description", description.clone()),
        ("synopsis", description),
        ("purl", url.clone()),
        ("comment", url),
        ("artist", text(artist)),
        ("genre", text(genre)),
        ("album", text(album)),
    ]
    .into_iter()
    .filter_map(|(key, value)| Some((key, value?)))
    .collect();
    Some(Facts { id, picks, tags, chapters: read_chapters(chapters) })
}

/// A metadata field's text: a string, or a list of them joined as yt-dlp joins one for ffmpeg
/// (`artists`); nothing for a field the video lacks, or one left empty — which yt-dlp does not
/// embed either.
fn text(field: &str) -> Option<String> {
    let value = json::parse(field)?;
    let joined = match &value {
        Json::String(text) => text.clone(),
        Json::Array(items) => items.iter().filter_map(Json::as_str).collect::<Vec<_>>().join(", "),
        _ => return None,
    };
    let joined = joined.replace('\0', "");
    (!joined.is_empty()).then_some(joined)
}

/// The chapters of a `%(chapters)j` list; none when the video has no chapters. A chapter without a
/// start or an end in seconds is no chapter a file can hold, and is passed over.
fn read_chapters(field: &str) -> Vec<Chapter> {
    let Some(list) = json::parse(field) else { return Vec::new() };
    list.items()
        .iter()
        .filter_map(|chapter| {
            Some(Chapter {
                start: chapter.get("start_time")?.as_f64()?,
                end: chapter.get("end_time")?.as_f64()?,
                title: chapter.get("title").and_then(Json::as_str).unwrap_or_default().to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A [`FACTS_FORMAT`] line, field by field.
    fn line(fields: [&str; 12]) -> String {
        fields.join("\t")
    }

    #[test]
    fn an_answer_holds_the_subtitle_plan_the_metadata_and_the_chapters() {
        let answer = line([
            "idA00000001",
            "ko",
            r#"{"en": [{"name": "English"}]}"#,
            r#"{"ko": [{"name": "Korean"}], "ko-orig": [{"name": "Korean (Original)"}]}"#,
            r#""Plash \"Speed\" Router""#,
            "20200215",
            r#""line one\nline two""#,
            "https://www.youtube.com/watch?v=idA00000001",
            r#""Bacon Now""#,
            r#"["People & Blogs"]"#,
            "NA",
            r#"[{"start_time": 0.0, "title": "Intro", "end_time": 12.5}, {"start_time": 12.5, "title": "Rest", "end_time": 171.13}]"#,
        ]);
        let facts = read_facts(&answer).expect("an answer");
        assert_eq!(facts.id, "idA00000001");
        let keys: Vec<(&str, bool)> = facts.picks.iter().map(|pick| (pick.key.as_str(), pick.auto)).collect();
        assert_eq!(keys, [("en", false), ("ko-orig", true)], "the uploader's English, and the native auto track");
        assert_eq!(
            facts.tags,
            [
                ("title", "Plash \"Speed\" Router".to_string()),
                ("date", "20200215".to_string()),
                ("description", "line one\nline two".to_string()),
                ("synopsis", "line one\nline two".to_string()),
                ("purl", "https://www.youtube.com/watch?v=idA00000001".to_string()),
                ("comment", "https://www.youtube.com/watch?v=idA00000001".to_string()),
                ("artist", "Bacon Now".to_string()),
                ("genre", "People & Blogs".to_string()),
            ],
            "every tag a download embeds that YouTube has a value for, and no album it lacks"
        );
        assert_eq!(
            facts.chapters,
            [
                Chapter { start: 0.0, end: 12.5, title: "Intro".into() },
                Chapter { start: 12.5, end: 171.13, title: "Rest".into() },
            ]
        );
    }

    #[test]
    fn a_video_lacking_fields_answers_with_what_it_has() {
        let sparse = line(["idB00000002", "NA", "{}", "{}", r#""t""#, "NA", r#""""#, "NA", r#"["A", "B"]"#, "NA", "NA", "NA"]);
        let facts = read_facts(&sparse).expect("an answer");
        assert!(facts.picks.is_empty() && facts.chapters.is_empty());
        assert_eq!(
            facts.tags,
            [("title", "t".to_string()), ("artist", "A, B".to_string())],
            "an empty description is no tag, and several artists read as yt-dlp joins them"
        );
        assert_eq!(read_facts("[youtube] idB00000002: Downloading webpage"), None, "chatter is no answer");
        assert_eq!(read_facts(&line(["NA", "", "", "", "", "", "", "", "", "", "", ""])), None, "no id, no answer");
    }

    #[test]
    fn every_video_without_an_answer_says_why_in_yt_dlps_words() {
        let ids: Vec<String> = ["idA00000001", "idB00000002", "idC00000003"].map(String::from).to_vec();
        let out = line(["idA00000001", "NA", "{}", "{}", r#""t""#, "NA", "NA", "NA", "NA", "NA", "NA", "NA"]);
        let err = "WARNING: [youtube] No supported JavaScript runtime\n\
                   ERROR: [youtube] idB00000002: Sign in to confirm you're not a bot\n\
                   ERROR: [youtube] idX: Unable to download API page: <urlopen error timed out>\n";
        let answers = read_answers(&ids, &out, err);
        assert_eq!(answers.facts.len(), 1);
        assert_eq!(
            answers.unanswered,
            [
                ("idB00000002".to_string(), "Sign in to confirm you're not a bot".to_string()),
                ("idC00000003".to_string(), "[youtube] idX: Unable to download API page: <urlopen error timed out>".to_string()),
            ],
            "its own error when yt-dlp named it, else the run's last"
        );
        let silent = read_answers(&ids[..1], "", "");
        assert_eq!(silent.unanswered, [("idA00000001".to_string(), "YouTube gave no answer".to_string())]);
    }
}
