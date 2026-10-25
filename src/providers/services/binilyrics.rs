use crate::{
    config::{PluginConfig, ProviderParams},
    ext::TrackInfoExt,
    providers::{LyricsProvider, ProviderResult, http::Http},
    types::{Lyrics, LyricsKind},
};
use nd_pdk::lyrics::TrackInfo;
use regex::{Captures, Regex};
use serde::Deserialize;
use std::time::Duration;
use std::{collections::HashMap, sync::LazyLock};

const BASE_URL: &str = "https://lyrics-api.binimum.org";

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<Record>,
}

#[derive(Debug, Deserialize)]
struct Record {
    #[serde(default)]
    album_name: String,
    duration: Option<u32>,
    #[serde(rename = "lyricsUrl")]
    lyrics_url: String,
}

pub struct BiniLyrics {
    romanization_as_translation: bool,
}

impl BiniLyrics {
    pub fn create(_: &ProviderParams) -> Box<dyn LyricsProvider> {
        Box::new(Self {
            romanization_as_translation: false,
        })
    }

    /// Variant for clients, such as Symfonium, that only expose the
    /// `translation` lyric track. The romanization is moved into that track so
    /// it is visible, while the original translation is discarded.
    pub fn create_romanized(_: &ProviderParams) -> Box<dyn LyricsProvider> {
        Box::new(Self {
            romanization_as_translation: true,
        })
    }

    fn track_lookup(&self, track: &TrackInfo) -> ProviderResult<Option<Record>> {
        let response = Http::get(BASE_URL)
            .param("track", track.title.as_str())
            .param("artist", track.all_artists())
            // Album is not enforced server-side.
            .param("album", track.album.as_str())
            .param("duration", track.duration().as_secs().to_string())
            .send()?;

        match response.status {
            200 => response
                .json::<SearchResponse>("track lookup")
                .map(|r| r.results.into_iter().next()),
            404 => Ok(None),
            429 => Err(response.rate_limited()),
            _ => Err(response.unexpected_status("the track lookup endpoint")),
        }
    }

    fn search(&self, track: &TrackInfo) -> ProviderResult<Vec<Record>> {
        let query = match track.first_artist() {
            Some(artist) => format!("{} {}", artist, track.clean_title()),
            None => track.clean_title(),
        };

        let response = Http::get(BASE_URL).param("q", query).send()?;

        match response.status {
            200 => response.json::<SearchResponse>("search").map(|r| r.results),
            404 => Ok(Vec::new()),
            429 => Err(response.rate_limited()),
            _ => Err(response.unexpected_status("the search endpoint")),
        }
    }

    fn fetch_ttml(&self, url: &str) -> ProviderResult<Option<Lyrics>> {
        let response = Http::get(url).send()?;

        match response.status {
            200 => {
                let text = response.text();
                let text = if self.romanization_as_translation {
                    move_romanization_to_translation(&text)
                } else {
                    text.into_owned()
                };
                Ok((!text.trim().is_empty()).then_some(Lyrics::Ttml(text)))
            }
            404 => Ok(None),
            429 => Err(response.rate_limited()),
            _ => Err(response.unexpected_status("the lyrics file")),
        }
    }
}

impl LyricsProvider for BiniLyrics {
    fn supported_kinds(&self) -> &'static [LyricsKind] {
        &[LyricsKind::Ttml]
    }

    fn fetch_lyrics(
        &self,
        track: &TrackInfo,
        cfg: &PluginConfig,
    ) -> ProviderResult<Option<Lyrics>> {
        if track.has_artist()
            && let Some(record) = self.track_lookup(track)?.filter(|r| matches(track, cfg, r))
        {
            return self.fetch_ttml(&record.lyrics_url);
        }

        match self
            .search(track)?
            .into_iter()
            .find(|record| matches(track, cfg, record))
        {
            Some(record) => self.fetch_ttml(&record.lyrics_url),
            None => Ok(None),
        }
    }
}

fn matches(track: &TrackInfo, cfg: &PluginConfig, record: &Record) -> bool {
    record.duration.is_some_and(|d| {
        track.matches_duration(Duration::from_secs(d.into()), cfg.duration_tolerance)
    }) && (!cfg.require_album_match || track.matches_album(&record.album_name))
}

#[derive(Debug, Clone, Copy)]
struct Block {
    start: usize,
    end: usize,
    inner_start: usize,
    inner_end: usize,
}

/// Finds a container element such as `<translations>...</translations>`.
/// Attributes and whitespace on the opening tag are tolerated.
fn find_block(text: &str, name: &str) -> Option<Block> {
    let open_prefix = format!("<{name}");
    let close = format!("</{name}>");
    let mut search_from = 0;

    while let Some(relative_start) = text[search_from..].find(&open_prefix) {
        let start = search_from + relative_start;
        let after_name = start + open_prefix.len();
        let next = text[after_name..].chars().next()?;

        if next == '>' || next.is_whitespace() {
            let open_end = after_name + text[after_name..].find('>')? + 1;
            let inner_end = open_end + text[open_end..].find(&close)?;
            return Some(Block {
                start,
                end: inner_end + close.len(),
                inner_start: open_end,
                inner_end,
            });
        }

        search_from = after_name;
    }

    None
}

fn retag_transliteration_as_translation(inner: &str) -> String {
    inner
        .replace("<transliteration", "<translation")
        .replace("</transliteration>", "</translation>")
}

static MAIN_PARAGRAPH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<p\b([^>]*)>(.*?)</p\s*>"#).expect("valid main paragraph regex")
});

static KEY_ATTRIBUTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:^|\s)(?:[a-z_][\w.-]*:)?key\s*=\s*["']([^"']+)["']"#)
        .expect("valid key attribute regex")
});

static TEXT_ELEMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<text\b([^>]*)>(.*?)</text\s*>"#).expect("valid text element regex")
});

static FOR_ATTRIBUTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:^|\s)for\s*=\s*["']([^"']+)["']"#).expect("valid for attribute regex")
});

static XML_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)<[^>]*>"#).expect("valid XML tag regex"));

/// Maps each main lyric line key to whether its text contains a CJK script.
///
/// BiniLyrics uses `lrc:key`, `itunes:key`, or `key` on `<p>` elements. The
/// parser matches the local attribute name so all three forms work.
fn main_line_needs_romanization(ttml: &str) -> HashMap<String, bool> {
    let mut lines = HashMap::new();

    for captures in MAIN_PARAGRAPH.captures_iter(ttml) {
        let Some(key) = attribute_value(&KEY_ATTRIBUTE, &captures[1]) else {
            continue;
        };

        let text = XML_TAG.replace_all(&captures[2], "");
        lines
            .entry(key.to_owned())
            .or_insert_with(|| has_cjk_script(&text));
    }

    lines
}

fn attribute_value<'a>(pattern: &Regex, attributes: &'a str) -> Option<&'a str> {
    pattern
        .captures(attributes)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().trim())
        .filter(|value| !value.is_empty())
}

/// Keeps romanization only for lines whose main text contains Hangul, kana, or
/// Han characters. Latin-only lines are already readable, so repeating them in
/// the translation track is noise for clients such as Symfonium.
fn filter_latin_only_romanization(
    romanization: &str,
    main_lines: &HashMap<String, bool>,
) -> String {
    TEXT_ELEMENT
        .replace_all(romanization, |captures: &Captures<'_>| {
            let Some(key) = attribute_value(&FOR_ATTRIBUTE, &captures[1]) else {
                return captures[0].to_owned();
            };

            match main_lines.get(key) {
                Some(false) => String::new(),
                _ => captures[0].to_owned(),
            }
        })
        .into_owned()
}

fn has_cjk_script(text: &str) -> bool {
    text.chars().any(is_cjk)
}

fn is_cjk(ch: char) -> bool {
    matches!(
        ch,
        '\u{1100}'..='\u{11ff}'       // Hangul Jamo
            | '\u{3130}'..='\u{318f}' // Hangul Compatibility Jamo
            | '\u{a960}'..='\u{a97f}' // Hangul Jamo Extended-A
            | '\u{d7b0}'..='\u{d7ff}' // Hangul Jamo Extended-B
            | '\u{ac00}'..='\u{d7a3}' // Hangul Syllables
            | '\u{3040}'..='\u{309f}' // Hiragana
            | '\u{30a0}'..='\u{30ff}' // Katakana
            | '\u{31f0}'..='\u{31ff}' // Katakana Phonetic Extensions
            | '\u{ff66}'..='\u{ff9d}' // Halfwidth Katakana
            | '\u{ffa0}'..='\u{ffdc}' // Halfwidth Hangul
            | '\u{3100}'..='\u{312f}' // Bopomofo
            | '\u{31a0}'..='\u{31bf}' // Bopomofo Extended
            | '\u{2e80}'..='\u{2eff}' // CJK Radicals Supplement
            | '\u{2f00}'..='\u{2fdf}' // Kangxi Radicals
            | '\u{3400}'..='\u{4dbf}' // CJK Extension A
            | '\u{4e00}'..='\u{9fff}' // CJK Unified Ideographs
            | '\u{f900}'..='\u{faff}' // CJK Compatibility Ideographs
            | '\u{20000}'..='\u{2fa1f}' // CJK Extensions B-F + Compatibility Supplement
            | '\u{30000}'..='\u{3134f}' // CJK Extensions G-H
    )
}

/// Matches a timed `<span>` whose text is immediately followed by punctuation
/// outside the span. BiniLyrics sometimes emits background ad-libs as
/// `(Ooh` inside the timed span and the closing `)` after `</span>`. Navidrome
/// then treats that orphan `)` as a gap belonging to the main voice, which
/// makes the cue line start with `)`.
static TIMED_SPAN_WITH_TRAILING_PUNCT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?s)(<span\b[^>]*\b(?:begin|end|dur)="[^"]*"[^>]*>)([^<]*)(</span>)([)\]}.,!?;:…”’]+)"#,
    )
    .expect("valid trailing punctuation regex")
});

/// Moves trailing punctuation into the preceding timed span when possible.
///
/// Closing brackets are only moved when the span contains the matching opening
/// bracket, so an already-balanced `(Ooh)</span>)` is left untouched. Other
/// trailing punctuation is moved as long as the span has text; punctuation
/// separated by whitespace is intentionally ignored by the regex.
fn move_trailing_punctuation(text: &str, punctuation: &str) -> (String, String) {
    if text.is_empty() {
        return (String::new(), punctuation.to_owned());
    }

    let mut moved = String::new();
    let mut remaining_start = 0;
    let mut updated = text.to_owned();

    for (index, ch) in punctuation.char_indices() {
        let movable = match ch {
            ')' => {
                updated.matches('(').count() > updated.matches(')').count()
                    || !updated.ends_with(')')
            }
            ']' => {
                updated.matches('[').count() > updated.matches(']').count()
                    || !updated.ends_with(']')
            }
            '}' => {
                updated.matches('{').count() > updated.matches('}').count()
                    || !updated.ends_with('}')
            }
            '”' => updated.matches('“').count() > updated.matches('”').count(),
            '’' => updated.matches('‘').count() > updated.matches('’').count(),
            _ => true,
        };
        if !movable {
            break;
        }

        updated.push(ch);
        moved.push(ch);
        remaining_start = index + ch.len_utf8();
    }

    (moved, punctuation[remaining_start..].to_owned())
}

fn normalize_orphan_trailing_punctuation(ttml: &str) -> String {
    TIMED_SPAN_WITH_TRAILING_PUNCT
        .replace_all(ttml, |captures: &Captures<'_>| {
            let text = &captures[2];
            let punctuation = &captures[4];
            let (moved, remaining) = move_trailing_punctuation(text, punctuation);
            if moved.is_empty() {
                captures[0].to_owned()
            } else {
                format!(
                    "{}{}{}{}{}",
                    &captures[1], text, moved, &captures[3], remaining
                )
            }
        })
        .into_owned()
}

/// Rewrites BiniLyrics TTML so clients that only understand `translation`
/// tracks receive the romanization instead of the English translation.
///
/// The original `<translations>` block is replaced, and the now-redundant
/// `<transliterations>` block is removed. Latin-only lines are omitted from the
/// romanization track because the transliteration of English is the English
/// text itself. If there is no romanization, the document is returned unchanged.
fn move_romanization_to_translation(text: &str) -> String {
    let Some(transliterations) = find_block(text, "transliterations") else {
        return text.to_owned();
    };

    let romanization = &text[transliterations.inner_start..transliterations.inner_end];
    if romanization.trim().is_empty() {
        return text.to_owned();
    }

    let romanization = normalize_orphan_trailing_punctuation(romanization);
    let main_lines = main_line_needs_romanization(text);
    let romanization = filter_latin_only_romanization(&romanization, &main_lines);
    let replacement = format!(
        "<translations>{}</translations>",
        retag_transliteration_as_translation(&romanization)
    );

    match find_block(text, "translations") {
        Some(translations) => apply_edits(
            text,
            vec![
                (translations.start, translations.end, replacement),
                (transliterations.start, transliterations.end, String::new()),
            ],
        ),
        None => {
            let mut out = String::with_capacity(text.len() + replacement.len());
            out.push_str(&text[..transliterations.start]);
            out.push_str(&replacement);
            out.push_str(&text[transliterations.end..]);
            out
        }
    }
}

fn apply_edits(text: &str, mut edits: Vec<(usize, usize, String)>) -> String {
    edits.sort_by_key(|(start, _, _)| *start);

    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end, replacement) in edits {
        if start < cursor {
            continue;
        }
        out.push_str(&text[cursor..start]);
        out.push_str(&replacement);
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_romanization_into_the_translation_track() {
        let input = concat!(
            r#"<tt><sourceMetadata><translations><translation xml:lang="en-US">"#,
            r#"<text for="L1">Hello</text></translation></translations>"#,
            r#"<transliterations><transliteration xml:lang="ko-Latn">"#,
            r#"<text for="L1">annyeong</text></transliteration></transliterations>"#,
            r#"</sourceMetadata></tt>"#
        );

        let output = move_romanization_to_translation(input);

        assert!(output.contains(
            r#"<translations><translation xml:lang="ko-Latn"><text for="L1">annyeong</text></translation></translations>"#
        ));
        assert!(!output.contains("transliterations"));
        assert!(!output.contains("Hello"));
    }

    #[test]
    fn creates_a_translation_track_when_there_was_none() {
        let input = concat!(
            r#"<tt><sourceMetadata><transliterations><transliteration xml:lang="ko-Latn">"#,
            r#"<text for="L1">annyeong</text></transliteration></transliterations>"#,
            r#"</sourceMetadata></tt>"#
        );

        let output = move_romanization_to_translation(input);

        assert!(output.contains(
            r#"<translations><translation xml:lang="ko-Latn"><text for="L1">annyeong</text></translation></translations>"#
        ));
        assert!(!output.contains("transliterations"));
    }

    #[test]
    fn folds_orphan_closing_parentheses_into_timed_spans() {
        let input = concat!(
            r#"<tt><sourceMetadata><translations><translation xml:lang="en-US">"#,
            r#"<text for="L1">Hello</text></translation></translations>"#,
            r#"<transliterations><transliteration xml:lang="ko-Latn">"#,
            r#"<text for="L1"><span ttm:role="x-bg"><span begin="9.111" end="9.500">(Ooh</span>)</span> "#,
            r#"<span begin="9.400" end="10.096">eo ri</span> "#,
            r#"<span ttm:role="x-bg"><span begin="20.000" end="20.500">(Yeah</span>)</span> "#,
            r#"<span begin="20.400" end="21.000">mal hae</span> "#,
            r#"<span ttm:role="x-bg"><span begin="30.000" end="30.500">(eo kkae</span>)</span> "#,
            r#"<span begin="30.400" end="31.000">sok</span> "#,
            r#"<span ttm:role="x-bg"><span begin="40.000" end="40.500">(som ssi e</span>)</span> "#,
            r#"<span begin="40.400" end="41.000">da</span></text>"#,
            r#"</transliteration></transliterations></sourceMetadata></tt>"#
        );

        let output = move_romanization_to_translation(input);

        assert!(output.contains(r#"<span begin="9.111" end="9.500">(Ooh)</span>"#));
        assert!(output.contains(r#"<span begin="20.000" end="20.500">(Yeah)</span>"#));
        assert!(output.contains(r#"<span begin="30.000" end="30.500">(eo kkae)</span>"#));
        assert!(output.contains(r#"<span begin="40.000" end="40.500">(som ssi e)</span>"#));
        assert!(!output.contains("(Ooh</span>)"));
        assert!(!output.contains("(Yeah</span>)"));
        assert!(!output.contains("(eo kkae</span>)"));
        assert!(!output.contains("(som ssi e</span>)"));
    }

    #[test]
    fn folds_trailing_punctuation_but_not_whitespace_separated_punctuation() {
        let input = concat!(
            r#"<tt><translations><translation xml:lang="en-US">"#,
            r#"<text for="L1">Hello</text></translation></translations>"#,
            r#"<transliterations><transliteration xml:lang="ko-Latn">"#,
            r#"<text for="L1"><span begin="1.000" end="2.000">word</span>, next "#,
            r#"<span begin="3.000" end="4.000">line</span> !</text>"#,
            r#"</transliteration></transliterations></tt>"#
        );

        let output = move_romanization_to_translation(input);

        assert!(output.contains(r#"<span begin="1.000" end="2.000">word,</span>"#));
        assert!(output.contains(r#"<span begin="3.000" end="4.000">line</span> !"#));
    }

    #[test]
    fn folds_a_closing_parenthesis_when_the_opening_is_outside_the_span() {
        let input = concat!(
            r#"<tt><translations><translation xml:lang="en-US">"#,
            r#"<text for="L1">Hello</text></translation></translations>"#,
            r#"<transliterations><transliteration xml:lang="ko-Latn">"#,
            r#"<text for="L1">(<span begin="1.000" end="2.000">Ooh</span>)</text>"#,
            r#"</transliteration></transliterations></tt>"#
        );

        let output = move_romanization_to_translation(input);

        assert!(output.contains(r#"<span begin="1.000" end="2.000">Ooh)</span>"#));
        assert!(!output.contains(r#"<span begin="1.000" end="2.000">Ooh</span>)"#));
    }

    #[test]
    fn does_not_add_an_extra_closing_parenthesis_when_already_balanced() {
        let input = concat!(
            r#"<tt><translations><translation xml:lang="en-US">"#,
            r#"<text for="L1">Hello</text></translation></translations>"#,
            r#"<transliterations><transliteration xml:lang="ko-Latn">"#,
            r#"<text for="L1"><span begin="1.000" end="2.000">(Ooh)</span>)</text>"#,
            r#"</transliteration></transliterations></tt>"#
        );

        let output = move_romanization_to_translation(input);

        assert!(output.contains(r#"(Ooh)</span>)"#));
        assert!(!output.contains(r#"(Ooh))</span>"#));
    }

    #[test]
    fn leaves_documents_without_romanization_unchanged() {
        let input = concat!(
            r#"<tt><sourceMetadata><translations><translation xml:lang="en-US">"#,
            r#"<text for="L1">Hello</text></translation></translations>"#,
            r#"</sourceMetadata></tt>"#
        );

        assert_eq!(move_romanization_to_translation(input), input);
    }

    #[test]
    fn tolerates_attributes_on_the_container_tags() {
        let input = concat!(
            r#"<tt><translations xml:lang="en-US"><translation xml:lang="en-US">"#,
            r#"<text for="L1">Hello</text></translation></translations>"#,
            r#"<transliterations xml:lang="ko-Latn"><transliteration xml:lang="ko-Latn">"#,
            r#"<text for="L1">annyeong</text></transliteration></transliterations></tt>"#
        );

        let output = move_romanization_to_translation(input);

        assert!(output.contains(
            r#"<translations><translation xml:lang="ko-Latn"><text for="L1">annyeong</text></translation></translations>"#
        ));
        assert!(!output.contains("transliterations"));
    }

    #[test]
    fn handles_romanization_blocks_before_translation_blocks() {
        let input = concat!(
            r#"<tt><transliterations><transliteration xml:lang="ko-Latn">"#,
            r#"<text for="L1">annyeong</text></transliteration></transliterations>"#,
            r#"<translations><translation xml:lang="en-US">"#,
            r#"<text for="L1">Hello</text></translation></translations></tt>"#
        );

        let output = move_romanization_to_translation(input);

        assert!(output.contains(
            r#"<translations><translation xml:lang="ko-Latn"><text for="L1">annyeong</text></translation></translations>"#
        ));
        assert!(!output.contains("transliterations"));
        assert!(!output.contains("Hello"));
    }

    #[test]
    fn filters_latin_only_lines_using_lrc_keys() {
        let input = concat!(
            r#"<tt><body>"#,
            r#"<p begin="1" end="2" lrc:key="L1">안녕하세요</p>"#,
            r#"<p begin="2" end="3" lrc:key="L2">Hello world</p>"#,
            r#"<p begin="3" end="4" lrc:key="L3">안녕 Hello</p>"#,
            r#"</body><head><metadata>"#,
            r#"<translations><translation xml:lang="en-US">"#,
            r#"<text for="L1">Hello</text>"#,
            r#"<text for="L2">Hello world</text>"#,
            r#"<text for="L3">Hello Hello</text>"#,
            r#"</translation></translations>"#,
            r#"<transliterations><transliteration xml:lang="ko-Latn">"#,
            r#"<text for="L1">annyeonghaseyo</text>"#,
            r#"<text for="L2">Hello world</text>"#,
            r#"<text for="L3">annyeong Hello</text>"#,
            r#"</transliteration></transliterations>"#,
            r#"</metadata></head></tt>"#
        );

        let output = move_romanization_to_translation(input);

        assert!(output.contains(r#"<text for="L1">annyeonghaseyo</text>"#));
        assert!(!output.contains(r#"<text for="L2">Hello world</text>"#));
        assert!(output.contains(r#"<text for="L3">annyeong Hello</text>"#));
        assert!(!output.contains("en-US"));
        assert!(!output.contains("transliterations"));
    }

    #[test]
    fn filters_latin_only_lines_using_itunes_keys() {
        let input = concat!(
            r#"<tt><body>"#,
            r#"<p itunes:key="L1">こんにちは</p>"#,
            r#"<p itunes:key="L2">漢字</p>"#,
            r#"<p itunes:key="L3">Hello</p>"#,
            r#"</body><head><metadata>"#,
            r#"<transliterations><transliteration xml:lang="ja-Latn">"#,
            r#"<text for="L1">konnichiwa</text>"#,
            r#"<text for="L2">kanji</text>"#,
            r#"<text for="L3">Hello</text>"#,
            r#"</transliteration></transliterations>"#,
            r#"</metadata></head></tt>"#
        );

        let output = move_romanization_to_translation(input);

        assert!(output.contains(r#"<text for="L1">konnichiwa</text>"#));
        assert!(output.contains(r#"<text for="L2">kanji</text>"#));
        assert!(!output.contains(r#"<text for="L3">Hello</text>"#));
    }

    #[test]
    fn keeps_transliteration_entries_without_a_matching_main_line() {
        let input = concat!(
            r#"<tt><body>"#,
            r#"<p key="L1">안녕</p>"#,
            r#"</body><head><metadata>"#,
            r#"<transliterations><transliteration xml:lang="ko-Latn">"#,
            r#"<text for="L1">annyeong</text>"#,
            r#"<text for="L9">unknown</text>"#,
            r#"</transliteration></transliterations>"#,
            r#"</metadata></head></tt>"#
        );

        let output = move_romanization_to_translation(input);

        assert!(output.contains(r#"<text for="L1">annyeong</text>"#));
        assert!(output.contains(r#"<text for="L9">unknown</text>"#));
    }

    #[test]
    fn drops_all_romanization_for_an_all_latin_song() {
        let input = concat!(
            r#"<tt><body>"#,
            r#"<p key="L1">Hello</p>"#,
            r#"<p key="L2">World</p>"#,
            r#"</body><head><metadata>"#,
            r#"<transliterations><transliteration xml:lang="ko-Latn">"#,
            r#"<text for="L1">Hello</text>"#,
            r#"<text for="L2">World</text>"#,
            r#"</transliteration></transliterations>"#,
            r#"</metadata></head></tt>"#
        );

        let output = move_romanization_to_translation(input);

        assert!(!output.contains("<text for="));
        assert!(!output.contains("transliterations"));
    }

    #[test]
    fn filters_latin_only_lines_without_breaking_punctuation_fold() {
        let input = concat!(
            r#"<tt><body>"#,
            r#"<p key="L1">(Ooh) 어리숙하다</p>"#,
            r#"<p key="L2">Hello</p>"#,
            r#"</body><head><metadata>"#,
            r#"<transliterations><transliteration xml:lang="ko-Latn">"#,
            r#"<text for="L1"><span begin="9.111" end="9.500">(Ooh</span>) "#,
            r#"<span begin="9.400" end="10.096">eo ri</span></text>"#,
            r#"<text for="L2">Hello</text>"#,
            r#"</transliteration></transliterations>"#,
            r#"</metadata></head></tt>"#
        );

        let output = move_romanization_to_translation(input);

        assert!(output.contains(r#"<span begin="9.111" end="9.500">(Ooh)</span>"#));
        assert!(!output.contains(r#"<text for="L2">Hello</text>"#));
    }
}
