use crate::{
    config::{PluginConfig, ProviderParams},
    ext::TrackInfoExt,
    providers::{LyricsProvider, ProviderResult, http::Http},
    types::{Lyrics, LyricsKind},
};
use nd_pdk::lyrics::TrackInfo;
use regex::{Captures, Regex};
use serde::Deserialize;
use std::sync::LazyLock;
use std::time::Duration;

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
                Ok((!text.trim().is_empty()).then(|| Lyrics::Ttml(text)))
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
/// `<transliterations>` block is removed. If there is no romanization, the
/// document is returned unchanged.
fn move_romanization_to_translation(text: &str) -> String {
    let Some(transliterations) = find_block(text, "transliterations") else {
        return text.to_owned();
    };

    let romanization = &text[transliterations.inner_start..transliterations.inner_end];
    if romanization.trim().is_empty() {
        return text.to_owned();
    }

    let romanization = normalize_orphan_trailing_punctuation(romanization);
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
}
