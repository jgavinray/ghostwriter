//! Deterministic ASD-STE100 checks — the verifiable half of the STE
//! integration. The prompt-side rules live in `prompts::STE_GUIDE`; this
//! module ships the part a small model must not be trusted with: the
//! controlled-dictionary lookups and the mechanical rules (no semicolons,
//! no contractions, sentence-length caps) that `ste_check` reports without
//! a model call.
//!
//! The data file `assets/ste100-unapproved.tsv` holds one row per
//! unapproved general word: word, part of speech, approved replacement
//! words (space-joined when the dictionary gives several). Rows beginning
//! with `#` are comments. The vocabulary pairs are data, not prose; the
//! spec's own text is never embedded here.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde::Serialize;

/// One violation: where it is, what was found, which rule it breaks, and
/// the approved fix.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    /// 1-based line in the submitted text.
    pub line: usize,
    /// The offending text, trimmed and clipped to one short quote.
    pub quote: String,
    /// Stable rule id, e.g. `ste-vocab`, `ste-semicolon`.
    pub rule: &'static str,
    /// What to use instead, in plain words.
    pub fix: String,
}

/// One row of the unapproved-word dictionary.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub word: String,
    pub pos: String,
    pub repl: String,
}

fn entries() -> Vec<Entry> {
    include_str!("../assets/ste100-unapproved.tsv")
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| {
            let mut cols = l.split('\t');
            let (word, pos, repl) = (cols.next()?, cols.next()?, cols.next()?);
            if word.is_empty() || repl.is_empty() {
                return None;
            }
            Some(Entry {
                word: word.to_string(),
                pos: pos.to_string(),
                repl: repl.to_string(),
            })
        })
        .collect()
}

static ENTRIES: LazyLock<Vec<Entry>> = LazyLock::new(entries);

/// Inflected spellings a word form can carry: the headword plus regular
/// verb/noun endings. Irregular forms are not generated; the model-side
/// guide covers them.
fn forms(word: &str, pos: &str) -> Vec<String> {
    let mut out = vec![word.to_string()];
    match pos {
        "v" => {
            out.push(format!("{word}s"));
            out.push(format!("{word}d"));
            if let Some(stem) = word.strip_suffix('e') {
                out.push(format!("{stem}ing"));
                out.push(format!("{stem}ed"));
                out.push(format!("{word}es"));
            } else {
                out.push(format!("{word}ing"));
                if word.ends_with("ss") {
                    out.push(format!("{word}es"));
                }
            }
        }
        "n" => {
            if word.ends_with('y') && !word.ends_with("ey") && !word.ends_with("oy") {
                out.push(format!("{}ies", word.strip_suffix('y').unwrap_or(word)));
            } else if word.ends_with(['s', 'x', 'z', 'h']) {
                out.push(format!("{word}es"));
            } else {
                out.push(format!("{word}s"));
            }
        }
        _ => {}
    }
    out
}

/// Lowercased inflected form -> dictionary entry. Built once.
static FORMS: LazyLock<HashMap<String, Entry>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    for entry in ENTRIES.iter() {
        for form in forms(&entry.word, &entry.pos) {
            map.entry(form.to_lowercase())
                .or_insert_with(|| entry.clone());
        }
    }
    map
});

/// Approved replacement text, lowercased and re-split for prose: "STOP GO"
/// reads better as "stop / go".
fn repl_display(repl: &str) -> String {
    repl.split_ascii_whitespace()
        .map(|w| w.to_lowercase())
        .collect::<Vec<_>>()
        .join(" / ")
}

pub fn dictionary_len() -> usize {
    ENTRIES.len()
}

/// Byte spans of word tokens (ASCII alphanumerics with internal hyphens,
/// apostrophes, and underscores).
fn word_spans(s: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (i, b) in s.bytes().enumerate() {
        let word_byte = b.is_ascii_alphanumeric() || b == b'-' || b == b'\'' || b == b'_';
        match (start, word_byte) {
            (None, true) => start = Some(i),
            (Some(s0), false) => {
                spans.push((s0, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s0) = start {
        spans.push((s0, s.len()));
    }
    spans
}

/// Mask the regions STE does not govern, in place on a char buffer of
/// identical length: fenced code blocks, inline code, URLs and URIs, and
/// path/filename-looking tokens. Byte offsets into the result still index
/// the original text.
fn mask(text: &str) -> Vec<char> {
    let mut buf: Vec<char> = text.chars().collect();
    let mut in_fence = false;
    let mut base = 0usize; // char offset of the current line
    for line in text.split('\n') {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            base += line.chars().count() + 1;
            continue;
        }
        if in_fence {
            for c in &mut buf[base..base + line.chars().count()] {
                *c = ' ';
            }
            base += line.chars().count() + 1;
            continue;
        }
        let line_chars: Vec<char> = line.chars().collect();
        let line_str: String = line_chars.iter().collect();
        let mut masked: Vec<char> = line_chars.clone();

        // inline code spans: `...`
        let bytes: Vec<char> = line_str.chars().collect();
        let mut i = 0;
        while let Some(rel) = bytes[i..].iter().position(|c| *c == '`') {
            let s = i + rel;
            let Some(rel_e) = bytes[s + 1..].iter().position(|c| *c == '`') else {
                break;
            };
            let e = s + 1 + rel_e;
            for c in &mut masked[s..=e] {
                *c = ' ';
            }
            i = e + 1;
            if i >= bytes.len() {
                break;
            }
        }

        // scheme:// URIs
        for scheme in [
            "http://", "https://", "ftp://", "kaibo://", "agent://", "file://",
        ] {
            let hay: String = masked.iter().collect();
            let mut search_from = 0;
            while let Some(pos) = hay[search_from..].find(scheme) {
                let pos = search_from + pos;
                let end = hay[pos..]
                    .find(char::is_whitespace)
                    .map(|i| pos + i)
                    .unwrap_or(hay.len());
                for (idx, c) in masked.iter_mut().enumerate() {
                    if idx >= pos && idx < end {
                        *c = ' ';
                    }
                }
                search_from = end;
            }
        }

        // filenames / paths: runs of path characters that carry a slash or a
        // dot-extension. Trailing dots stay unmasked so they can still end a
        // sentence ("...initiate.md." keeps its period).
        let is_path_char =
            |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | '\\' | '~');
        let mut j = 0;
        while j < masked.len() {
            if !is_path_char(masked[j]) {
                j += 1;
                continue;
            }
            let s2 = j;
            let mut e2 = j;
            while e2 < masked.len() && is_path_char(masked[e2]) {
                e2 += 1;
            }
            while e2 > s2 && masked[e2 - 1] == '.' {
                e2 -= 1;
            }
            let tok: String = masked[s2..e2].iter().collect();
            let dot_ext = tok
                .rfind('.')
                .map(|k| {
                    let ext = &tok[k + 1..];
                    k >= 2
                        && ext.len() >= 2
                        && ext.len() <= 8
                        && ext.chars().all(|c| c.is_ascii_alphabetic())
                })
                .unwrap_or(false);
            if (tok.contains('/') || tok.contains('\\') || dot_ext)
                && tok.chars().any(|c| c.is_ascii_alphanumeric())
            {
                for c in &mut masked[s2..e2] {
                    *c = ' ';
                }
            }
            j = e2.max(s2 + 1);
        }

        for (k, c) in masked.into_iter().enumerate() {
            buf[base + k] = c;
        }
        base += line.chars().count() + 1;
    }
    buf
}

fn clip(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > 60 {
        let cut: String = s.chars().take(57).collect();
        format!("{}...", cut.trim_end())
    } else {
        s.to_string()
    }
}

const LATIN: &[(&str, &str)] = &[
    ("etc.", "use and so on"),
    ("e.g.", "use for example"),
    ("eg.", "use for example"),
    ("i.e.", "use that is"),
    ("vs.", "use compared with"),
    ("viz.", "use namely"),
];

const CONTRACTIONS: &[(&str, &str)] = &[
    ("don't", "do not"),
    ("doesn't", "does not"),
    ("didn't", "did not"),
    ("can't", "cannot"),
    ("won't", "will not"),
    ("isn't", "is not"),
    ("aren't", "are not"),
    ("wasn't", "was not"),
    ("weren't", "were not"),
    ("haven't", "have not"),
    ("hasn't", "has not"),
    ("hadn't", "had not"),
    ("it's", "it is"),
    ("there's", "there is"),
    ("here's", "here is"),
    ("that's", "that is"),
    ("what's", "what is"),
    ("let's", "let us"),
    ("you're", "you are"),
    ("we're", "we are"),
    ("they're", "they are"),
    ("i'm", "I am"),
    ("you'll", "you will"),
    ("we'll", "we will"),
    ("they'll", "they will"),
    ("it'll", "it will"),
    ("you've", "you have"),
    ("we've", "we have"),
    ("they've", "they have"),
    ("i've", "I have"),
    ("you'd", "you would"),
    ("we'd", "we would"),
    ("they'd", "they would"),
    ("i'd", "I would"),
    ("he's", "he is"),
    ("she's", "she is"),
];

/// Sentence-length caps from the spec: 20 words for an instruction, 25 for
/// descriptive prose. A sentence whose first word is not a subject marker
/// is treated as an instruction.
const MAX_INSTRUCTION_WORDS: usize = 20;
const MAX_DESCRIBE_WORDS: usize = 25;

fn subject_initial(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "the"
            | "a"
            | "an"
            | "this"
            | "these"
            | "that"
            | "those"
            | "i"
            | "we"
            | "you"
            | "he"
            | "she"
            | "it"
            | "they"
            | "our"
            | "your"
            | "his"
            | "her"
            | "its"
            | "their"
            | "there"
            | "here"
            | "each"
            | "every"
            | "some"
            | "many"
            | "most"
            | "all"
            | "no"
            | "both"
            | "however"
            | "also"
            | "when"
            | "while"
            | "after"
            | "before"
            | "if"
            | "in"
            | "on"
            | "at"
            | "to"
            | "for"
            | "with"
            | "by"
    )
}

/// STE word count: each number, unit, abbreviation, alphanumeric
/// identifier, quoted run, parenthesized run, and hyphenated pair counts
/// as one word.
fn ste_words(s: &str) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let mut count = 0;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_alphanumeric() {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric()
                    || chars[i] == '-'
                    || (chars[i] == '.'
                        && i + 1 < chars.len()
                        && chars[i + 1].is_ascii_alphanumeric()))
            {
                i += 1;
            }
            if chars[start..i].iter().any(|c| c.is_ascii_alphanumeric()) {
                count += 1;
            }
        } else if c == '(' {
            let mut depth = 1;
            i += 1;
            while i < chars.len() && depth > 0 {
                match chars[i] {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                i += 1;
            }
            count += 1;
        } else if c == '"' || c == '\u{201c}' {
            while i < chars.len() && chars[i] != '"' && chars[i] != '\u{201d}' {
                i += 1;
            }
            i += 1;
            count += 1;
        } else {
            i += 1;
        }
    }
    count
}

fn find_word(hay: &str, needle: &str) -> Option<usize> {
    hay.match_indices(needle).find_map(|(i, _)| {
        let before_ok = i == 0
            || !hay[..i]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        let after = hay[i + needle.len()..].chars().next();
        let after_ok = !after.is_some_and(|c| c.is_ascii_alphanumeric());
        if before_ok && after_ok {
            Some(i)
        } else {
            None
        }
    })
}

/// Join a paragraph's masked lines, split into sentences, and report each
/// with the 1-based line its first character sits on.
fn collect_sentences(para: &[(usize, String)]) -> Vec<(usize, String)> {
    if para.is_empty() {
        return Vec::new();
    }
    // char offset within the joined text -> paragraph index, so quotes map
    // back to lines
    let mut joined = String::new();
    let mut bounds: Vec<(usize, usize)> = Vec::new(); // (join start, line)
    for (line, l) in para {
        bounds.push((joined.chars().count(), *line));
        joined.push_str(l.trim());
        joined.push(' ');
    }
    let chars: Vec<char> = joined.chars().collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        if matches!(chars[i], '.' | '!' | '?') {
            let prev_alnum = i > 0 && (chars[i - 1].is_ascii_alphanumeric() || chars[i - 1] == ')');
            let body: String = chars[start..i].iter().collect();
            let not_step_label = body.trim().chars().count() > 1
                && !body.trim().chars().next().is_some_and(|c| {
                    c.is_ascii_lowercase() && i - start == body.trim().chars().count() + 1
                });
            if prev_alnum && not_step_label {
                let sentence: String = chars[start..=i].iter().collect();
                let sentence = sentence.trim().to_string();
                if !sentence.is_empty() {
                    out.push((line_for(&bounds, start), sentence));
                }
                start = i + 1;
            }
        }
        i += 1;
    }
    let tail: String = chars[start..].iter().collect();
    let tail = tail.trim();
    if tail.chars().count() > 2 {
        out.push((line_for(&bounds, start), tail.to_string()));
    }
    out
}

fn line_for(bounds: &[(usize, usize)], char_pos: usize) -> usize {
    let mut line = bounds.first().map(|b| b.1).unwrap_or(1);
    for (start, l) in bounds {
        if *start <= char_pos {
            line = *l;
        } else {
            break;
        }
    }
    line
}

/// Run every deterministic STE check over `text`.
pub fn check(text: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    let masked = mask(text);
    let masked_text: String = masked.iter().collect();

    let orig_lines: Vec<&str> = text.split('\n').collect();
    let masked_lines: Vec<&str> = masked_text.split('\n').collect();

    for (idx, mline) in masked_lines.iter().enumerate() {
        let lineno = idx + 1;
        let oline = orig_lines.get(idx).copied().unwrap_or("");
        let ochars: Vec<char> = oline.chars().collect();
        for (s, e) in word_spans(mline) {
            // word_spans yields byte spans; the ASCII-only token boundary
            // makes byte and char offsets agree for alnum tokens, but the
            // line may hold multibyte text before the token — recompute in
            // char space over the masked line.
            let mchars: Vec<char> = mline.chars().collect();
            let mut char_span = (s, e);
            if !mline.is_ascii() {
                let mut cs = None;
                let mut ce = None;
                let mut count = 0;
                for (k, c) in mline.char_indices() {
                    if k == s {
                        cs = Some(count);
                    }
                    count += c.len_utf8();
                    if k + c.len_utf8() == e {
                        ce = Some(count);
                    }
                }
                if let (Some(cs), Some(ce)) = (cs, ce) {
                    char_span = (cs, ce);
                }
            }
            let (cs, ce) = char_span;
            if ce > mchars.len() {
                continue;
            }
            let tok: String = mchars[cs..ce].iter().collect();
            let key = tok.trim_matches('\'').to_lowercase();
            if let Some(entry) = FORMS.get(&key) {
                let quote: String = ochars[cs..ce.min(ochars.len())].iter().collect();
                findings.push(Finding {
                    line: lineno,
                    quote: clip(&quote),
                    rule: "ste-vocab",
                    fix: format!(
                        "use {} (unapproved {} word: {key})",
                        repl_display(&entry.repl),
                        entry.pos
                    ),
                });
            }
        }
        let hay = mline.to_lowercase();
        for (needle, fix) in LATIN {
            if let Some(pos) = find_word(&hay, needle) {
                let quote: String = ochars
                    .iter()
                    .enumerate()
                    .filter(|(k, _)| *k >= pos && *k < pos + needle.chars().count())
                    .map(|(_, c)| *c)
                    .collect();
                findings.push(Finding {
                    line: lineno,
                    quote: clip(&quote),
                    rule: "ste-latin",
                    fix: (*fix).to_string(),
                });
            }
        }
        for (needle, fix) in CONTRACTIONS {
            if let Some(pos) = find_word(&hay, needle) {
                let quote: String = ochars
                    .iter()
                    .enumerate()
                    .filter(|(k, _)| *k >= pos && *k < pos + needle.chars().count())
                    .map(|(_, c)| *c)
                    .collect();
                findings.push(Finding {
                    line: lineno,
                    quote: clip(&quote),
                    rule: "ste-contraction",
                    fix: (*fix).to_string(),
                });
            }
        }
        if hay.contains(';') {
            findings.push(Finding {
                line: lineno,
                quote: clip(oline),
                rule: "ste-semicolon",
                fix: "write two sentences; the semicolon is not approved in STE".into(),
            });
        }
    }

    // Sentence lengths over paragraph runs of masked prose.
    let mut para: Vec<(usize, String)> = Vec::new();
    let flush = |para: &mut Vec<(usize, String)>, findings: &mut Vec<Finding>| {
        for (line, sentence) in collect_sentences(para) {
            let words = ste_words(&sentence);
            let first = sentence
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_matches(|c: char| !c.is_alphanumeric());
            let limit = if first.is_empty() || subject_initial(first) {
                MAX_DESCRIBE_WORDS
            } else {
                MAX_INSTRUCTION_WORDS
            };
            if words > limit {
                findings.push(Finding {
                    line,
                    quote: clip(&sentence),
                    rule: "ste-sentence-length",
                    fix: format!("split the sentence: {words} words, maximum is {limit}"),
                });
            }
        }
        para.clear();
    };
    for (idx, mline) in masked_lines.iter().enumerate() {
        let t = mline.trim();
        if t.is_empty() || t.starts_with('#') {
            flush(&mut para, &mut findings);
            continue;
        }
        para.push((idx + 1, mline.to_string()));
    }
    flush(&mut para, &mut findings);

    findings.sort_by_key(|f| (f.line, f.rule, f.quote.clone()));
    findings.dedup_by_key(|f| (f.line, f.rule, f.quote.clone()));
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionary_loads() {
        assert!(
            dictionary_len() > 900,
            "dictionary too small: {}",
            dictionary_len()
        );
        assert!(FORMS.contains_key("utilize"));
        assert!(FORMS.contains_key("utilized"));
        assert!(FORMS.contains_key("utilizing"));
        let e = FORMS.get("initiate").expect("initiate missing");
        assert_eq!(e.word, "initiate");
        assert!(e.repl.contains("START"), "odd replacement {:?}", e.repl);
    }

    #[test]
    fn finds_unapproved_vocabulary() {
        let f = check("The system utilizes the sensor to initiate a cycle.");
        let quotes: Vec<&str> = f.iter().map(|x| x.quote.as_str()).collect();
        assert!(quotes.contains(&"utilizes"), "{quotes:?}");
        assert!(quotes.contains(&"initiate"), "{quotes:?}");
        assert!(f.iter().all(|x| x.rule == "ste-vocab"), "{f:?}");
    }

    #[test]
    fn code_and_paths_are_exempt() {
        // utilize/initiate appear only inside exempt regions: inline code,
        // a fenced block, a URL, and a path.
        let f = check("Read https://example.com/utilize-guide before you open /srv/initiate.md.\n\n`utilize()` and `initiate()` are exempt inline.\n\n```rust\nutilize(initiate);\n```\n");
        let leaked: Vec<&Finding> = f
            .iter()
            .filter(|x| x.quote.contains("utilize") || x.quote.contains("initiate"))
            .collect();
        assert!(leaked.is_empty(), "code was flagged: {leaked:?}");
    }

    #[test]
    fn finds_semicolons_latin_and_contractions() {
        let f = check(
            "Remove the bolt; do not strip the thread.\nCheck the settings, e.g. the port.\nIt doesn't work.",
        );
        assert!(f.iter().any(|x| x.rule == "ste-semicolon"));
        assert!(f.iter().any(|x| x.rule == "ste-latin"));
        assert!(f.iter().any(|x| x.rule == "ste-contraction"));
    }

    #[test]
    fn finds_long_sentences() {
        let long = "The first thing happened and then the second thing continued and the third thing also went on for a very long time while the operator watched the display carefully and wrote down every number that appeared.";
        let f = check(long);
        assert!(f.iter().any(|x| x.rule == "ste-sentence-length"), "{f:?}");
    }

    #[test]
    fn short_sentences_pass() {
        let f = check("Remove the bolt. Check the seal. Reinstall the cover.");
        assert!(!f.iter().any(|x| x.rule == "ste-sentence-length"), "{f:?}");
    }

    #[test]
    fn word_count_groups_per_spec() {
        // numbers, hyphenated pairs, and parenthesized runs each count as one
        assert_eq!(ste_words("Set the torque to 12 N-m (9 lbf-ft) now."), 8);
    }
}
