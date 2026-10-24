//! Reading JSON — the little of it this crate meets: the fields a yt-dlp print template asks for
//! (`%(subtitles)j`, a description, a video's chapters) and ffprobe's account of one file. A
//! reader only, holding values as they come, since nothing here writes JSON and what is read is
//! small.

use std::iter::Peekable;
use std::str::Chars;

/// One JSON value. An object keeps its members in the order they were written, as a caption map's
/// languages are read in order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    /// The member named `key`, of an object.
    pub(crate) fn get(&self, key: &str) -> Option<&Json> {
        self.members().iter().find(|(name, _)| name == key).map(|(_, value)| value)
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(text) => Some(text),
            _ => None,
        }
    }

    pub(crate) fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Number(number) => Some(*number),
            _ => None,
        }
    }

    /// An array's items; none, for anything else.
    pub(crate) fn items(&self) -> &[Json] {
        match self {
            Json::Array(items) => items,
            _ => &[],
        }
    }

    /// An object's members; none, for anything else.
    pub(crate) fn members(&self) -> &[(String, Json)] {
        match self {
            Json::Object(members) => members,
            _ => &[],
        }
    }
}

/// `text` read as one JSON value, whitespace around it allowed; `None` when it is not one — which
/// includes yt-dlp's `NA` for a field a video does not have.
pub(crate) fn parse(text: &str) -> Option<Json> {
    let mut reader = Reader { chars: text.chars().peekable() };
    let value = reader.value()?;
    reader.skip_whitespace();
    reader.chars.peek().is_none().then_some(value)
}

struct Reader<'a> {
    chars: Peekable<Chars<'a>>,
}

impl Reader<'_> {
    fn skip_whitespace(&mut self) {
        while self.chars.next_if(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r')).is_some() {}
    }

    fn value(&mut self) -> Option<Json> {
        self.skip_whitespace();
        match self.chars.peek()? {
            '{' => self.object(),
            '[' => self.array(),
            '"' => self.string().map(Json::String),
            't' => self.literal("true", Json::Bool(true)),
            'f' => self.literal("false", Json::Bool(false)),
            'n' => self.literal("null", Json::Null),
            _ => self.number(),
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Option<Json> {
        word.chars().all(|expected| self.chars.next() == Some(expected)).then_some(value)
    }

    fn number(&mut self) -> Option<Json> {
        let mut text = String::new();
        while let Some(ch) = self.chars.next_if(|ch| ch.is_ascii_digit() || matches!(ch, '-' | '+' | '.' | 'e' | 'E')) {
            text.push(ch);
        }
        text.parse().ok().map(Json::Number)
    }

    fn string(&mut self) -> Option<String> {
        self.chars.next(); // the opening quote
        let mut text = String::new();
        loop {
            match self.chars.next()? {
                '"' => return Some(text),
                '\\' => text.push(match self.chars.next()? {
                    '"' => '"',
                    '\\' => '\\',
                    '/' => '/',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    'u' => {
                        self.unicode_escape(&mut text)?;
                        continue;
                    }
                    _ => return None,
                }),
                ch => text.push(ch),
            }
        }
    }

    /// The character a `\u` escape spells, pushed onto `text`: a pair of escapes for one outside
    /// the Basic Multilingual Plane (an emoji in a title), as JSON writes those. Half a pair reads
    /// as U+FFFD, as a lossy decode would have it, and whatever follows it is kept.
    fn unicode_escape(&mut self, text: &mut String) -> Option<()> {
        let high = self.hex4()?;
        if !(0xD800..0xDC00).contains(&high) {
            text.push(char::from_u32(high).unwrap_or(char::REPLACEMENT_CHARACTER));
            return Some(());
        }
        let paired = self.chars.clone().take(2).eq(['\\', 'u']);
        if !paired {
            text.push(char::REPLACEMENT_CHARACTER);
            return Some(());
        }
        self.chars.nth(1); // the `\u` just seen
        let low = self.hex4()?;
        if (0xDC00..0xE000).contains(&low) {
            let pair = char::from_u32(0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00));
            text.push(pair.unwrap_or(char::REPLACEMENT_CHARACTER));
        } else {
            text.push(char::REPLACEMENT_CHARACTER);
            text.push(char::from_u32(low).unwrap_or(char::REPLACEMENT_CHARACTER));
        }
        Some(())
    }

    fn hex4(&mut self) -> Option<u32> {
        (0..4).try_fold(0, |code, _| Some(code * 16 + self.chars.next()?.to_digit(16)?))
    }

    fn array(&mut self) -> Option<Json> {
        self.chars.next(); // [
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.chars.next_if_eq(&']').is_some() {
            return Some(Json::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.skip_whitespace();
            match self.chars.next()? {
                ',' => {}
                ']' => return Some(Json::Array(items)),
                _ => return None,
            }
        }
    }

    fn object(&mut self) -> Option<Json> {
        self.chars.next(); // {
        let mut members = Vec::new();
        self.skip_whitespace();
        if self.chars.next_if_eq(&'}').is_some() {
            return Some(Json::Object(members));
        }
        loop {
            self.skip_whitespace();
            if self.chars.peek() != Some(&'"') {
                return None;
            }
            let key = self.string()?;
            self.skip_whitespace();
            if self.chars.next()? != ':' {
                return None;
            }
            members.push((key, self.value()?));
            self.skip_whitespace();
            match self.chars.next()? {
                ',' => {}
                '}' => return Some(Json::Object(members)),
                _ => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_of_every_kind_read_back_as_written() {
        let read = parse(r#" {"list": [1, -2.5e3, true, false, null], "text": "x", "empty": {}, "none": []} "#);
        let read = read.expect("valid JSON");
        assert_eq!(
            read.get("list").map(Json::items),
            Some(&[Json::Number(1.0), Json::Number(-2500.0), Json::Bool(true), Json::Bool(false), Json::Null][..])
        );
        assert_eq!(read.get("text").and_then(Json::as_str), Some("x"));
        assert!(read.get("empty").is_some_and(|empty| empty.members().is_empty()));
        assert!(read.get("none").is_some_and(|none| none.items().is_empty()));
        assert_eq!(read.get("absent"), None);
        let keys: Vec<&str> = read.members().iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["list", "text", "empty", "none"], "members stay in the order written");
    }

    #[test]
    fn strings_decode_every_escape_json_writes() {
        // Escaped as Python's json.dumps escapes them, which is what yt-dlp's `j` conversion is:
        // an emoji outside the Basic Multilingual Plane as a surrogate pair.
        let text = parse(r#""tab\there\nnew \"quoted\" \\ \/ \u00e9 \ud83d\ude00 \b\f\r""#);
        assert_eq!(text.as_ref().and_then(Json::as_str), Some("tab\there\nnew \"quoted\" \\ / é 😀 \u{8}\u{c}\r"));
        // Half a pair stands for a character it cannot spell; what follows it is kept.
        assert_eq!(parse(r#""\ud83d alone""#).as_ref().and_then(Json::as_str), Some("\u{FFFD} alone"));
        assert_eq!(parse(r#""\ud83dA""#).as_ref().and_then(Json::as_str), Some("\u{FFFD}A"));
        assert_eq!(parse(r#""\ud83d\u0041""#).as_ref().and_then(Json::as_str), Some("\u{FFFD}A"));
        assert_eq!(parse(r#""raw ünïcödé""#).as_ref().and_then(Json::as_str), Some("raw ünïcödé"));
    }

    #[test]
    fn what_is_no_json_reads_as_nothing() {
        for text in ["NA", "", "{", "[1,]", "[1 2]", r#"{"a" 1}"#, r#"{a: 1}"#, r#""unterminated"#, "{} x", r#""\q""#, r#""\u12""#, "tru", "-", "1e"] {
            assert_eq!(parse(text), None, "{text:?}");
        }
    }
}
