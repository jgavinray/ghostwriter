//! Deterministic mechanical checks for the registered writing standards —
//! the verifiable half that `style_check` reports without a model call. The
//! prompt-side rules live in `prompts`; the registry that binds a style to
//! its guide and its [`Rules`] lives in `crate::styles`. This module ships
//! the part a small model must not be trusted with: dictionary lookups and
//! the line-level rules (semicolons, contractions, Latin abbreviations,
//! length caps) run through one engine that takes its policy from a
//! [`Rules`] set.
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
    /// Stable rule id: the engine's prefix plus the primitive (`ste-vocab`,
    /// `google-latin`, `microsoft-exclamation`) or a full id from a
    /// style's own word table (`google-tone`, `microsoft-bias`).
    pub rule: String,
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

/// Char spans of word tokens (ASCII alphanumerics with internal hyphens,
/// apostrophes, and underscores). Spans index the string's `chars()`, so
/// callers that hold a `Vec<char>` of the same text can slice it directly —
/// byte offsets would drift past every multibyte character in the line.
fn word_spans(s: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in s.chars().enumerate() {
        let word_char = c.is_ascii_alphanumeric() || c == '-' || c == '\'' || c == '_';
        match (start, word_char) {
            (None, true) => start = Some(i),
            (Some(s0), false) => {
                spans.push((s0, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s0) = start {
        spans.push((s0, s.chars().count()));
    }
    spans
}

/// Mask the regions STE does not govern, in place on a char buffer of
/// identical length: fenced code blocks, inline code, URLs and URIs, and
/// path/filename-looking tokens. Char indices into the result index the
/// original text's `chars()` 1:1 — masking only replaces characters with
/// spaces, never removes them.
///
/// An unclosed fence exempts the rest of the document BY DESIGN, matching
/// CommonMark's treatment of an unterminated code fence.
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
        let mut masked: Vec<char> = line_chars.clone();

        // inline code spans: `...` (char indices into the line buffer)
        let mut i = 0;
        while let Some(rel) = line_chars[i..].iter().position(|c| *c == '`') {
            let s = i + rel;
            let Some(rel_e) = line_chars[s + 1..].iter().position(|c| *c == '`') else {
                break;
            };
            let e = s + 1 + rel_e;
            for c in &mut masked[s..=e] {
                *c = ' ';
            }
            i = e + 1;
            if i >= line_chars.len() {
                break;
            }
        }

        // scheme:// URIs: one left-to-right pass over the masked char
        // buffer. The scheme is generic — `[A-Za-z][A-Za-z0-9+.-]*`
        // immediately before `://` — so there is no per-scheme table (and
        // no private-codename schemes in public source), and no per-hit
        // haystack rebuild: the old loop re-collected a String for every
        // scheme and every hit, which measured quadratic on a long single
        // line (N6). Indices stay char indices of the masked buffer, which
        // is parallel to the original line's chars (m2).
        let is_scheme_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.');
        let mut k = 0usize;
        while k < masked.len() {
            if !masked[k].is_ascii_alphanumeric() {
                k += 1;
                continue;
            }
            let head = k;
            while k < masked.len() && is_scheme_char(masked[k]) {
                k += 1;
            }
            let scheme_head = masked[head].is_ascii_alphabetic();
            if scheme_head
                && k + 2 < masked.len()
                && masked[k] == ':'
                && masked[k + 1] == '/'
                && masked[k + 2] == '/'
            {
                let mut e = k + 3;
                while e < masked.len() && !masked[e].is_whitespace() {
                    e += 1;
                }
                for c in &mut masked[head..e] {
                    *c = ' ';
                }
                k = e;
            }
        }

        // filenames / paths: runs of path characters, masked only when the
        // token looks like a path. Trailing dots stay unmasked so they can
        // still end a sentence ("...initiate.md." keeps its period).
        //
        // A bare word joined by a SINGLE slash is not a path — masking it
        // used to make every slash-containing token invisible to the word
        // rules, so bias pairs like `master/slave` were never reported.
        // Such a token reaches the word rules (each side is a separate
        // word span); it is blanked only with a file extension, two or
        // more slashes, a backslash, a tilde, or a known directory head.
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
            let slashes = tok.matches('/').count();
            let head = tok
                .trim_start_matches('/')
                .split('/')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let looks_like_path = dot_ext
                || tok.contains('\\')
                || tok.contains('~')
                || slashes >= 2
                || (slashes == 1 && DIR_HEADS.contains(&head.as_str()));
            if looks_like_path && tok.chars().any(|c| c.is_ascii_alphanumeric()) {
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

/// Leading directory names that mark a single-slash token as a path even
/// without an extension ("src/main", "/etc/passwd"). Deliberately boring:
/// only names a running token cannot plausibly be as prose — the heads of
/// bias pairs like `master/slave` are absent, so those reach the word rules.
const DIR_HEADS: &[&str] = &[
    "bin", "dev", "doc", "docs", "etc", "home", "lib", "libs", "opt", "sbin", "src", "srv", "tmp",
    "usr", "var",
];

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

/// First occurrence of the (ASCII) `needle` in the lowercased masked-line
/// char buffer, aligned to word boundaries: an alphanumeric neighbour on
/// either side disqualifies the match, while `-` and `/` inside a needle
/// stay word characters (see [`WordRule`]). Returns a CHAR index — the
/// buffer is built with ASCII-only lowering, so positions map 1:1 onto the
/// masked line and the original line.
fn find_word(hay: &[char], needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    for i in 0..=hay.len() - needle.len() {
        if hay[i..i + needle.len()] != needle[..] {
            continue;
        }
        let before_ok = i == 0 || !hay[i - 1].is_ascii_alphanumeric();
        let after = hay.get(i + needle.len()).copied();
        let after_ok = !after.is_some_and(|c| c.is_ascii_alphanumeric());
        if before_ok && after_ok {
            return Some(i);
        }
    }
    None
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
            // N5: any script counts — a terminator after é or 漢 ends the
            // sentence just like one after an ASCII letter.
            let prev_alnum = i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == ')');
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

/// Which mechanical primitives a registered style runs, and which extra
/// word rules it bans. Everything matches at word boundaries or line level
/// over masked prose, so code, inline code, paths, and URLs stay exempt
/// under every style. A rule a style does not enable is not reported —
/// what the checker stays silent on remains the model's judgment.
#[derive(Debug)]
pub struct Rules {
    /// Prefix for the engine's own rule ids: `ste`, `google`, `microsoft`.
    pub prefix: &'static str,
    /// The ASD-STE100 unapproved-word dictionary lookup.
    pub dictionary: bool,
    /// Latin abbreviations (`etc.`, `e.g.`, `i.e.` ...).
    pub latin: bool,
    /// Contractions, with the spelled-out form as the fix.
    pub contractions: bool,
    /// Semicolons: one finding per line.
    pub semicolon: bool,
    /// Exclamation marks: one finding per line.
    pub exclamation: bool,
    /// The ASD-STE100 sentence-length caps (20 / 25).
    pub length_caps: bool,
    /// Style-specific banned needles, matched against the lowercased line.
    pub words: &'static [WordRule],
}

/// One banned word or phrase from a style's own table.
#[derive(Debug)]
pub struct WordRule {
    /// Lowercased needle matched at word boundaries; may contain spaces,
    /// `/`, or `-` (those count as word characters here).
    pub needle: &'static str,
    /// Full stable rule id, e.g. `google-tone`.
    pub rule: &'static str,
    /// What to use instead, in plain words.
    pub fix: &'static str,
}

/// The ASD-STE100 mechanical set: the verifiable half of the standard.
pub static STE_RULES: Rules = Rules {
    prefix: "ste",
    dictionary: true,
    latin: true,
    contractions: true,
    semicolon: true,
    exclamation: false,
    length_caps: true,
    words: &[],
};

/// Run every deterministic STE check over `text`.
pub fn check(text: &str) -> Vec<Finding> {
    check_with(text, &STE_RULES)
}

/// Run one registered style's mechanical rules over `text`. Pure: the
/// findings are a function of the text and the rule set alone, so tests
/// can pin exact findings per style.
pub fn check_with(text: &str, rules: &Rules) -> Vec<Finding> {
    let mut findings = Vec::new();
    let masked = mask(text);
    let masked_text: String = masked.iter().collect();

    let orig_lines: Vec<&str> = text.split('\n').collect();
    let masked_lines: Vec<&str> = masked_text.split('\n').collect();

    for (idx, mline) in masked_lines.iter().enumerate() {
        let lineno = idx + 1;
        let oline = orig_lines.get(idx).copied().unwrap_or("");
        let ochars: Vec<char> = oline.chars().collect();
        // One lowercased char buffer per masked line, ASCII-only lowering:
        // length-preserving BY CONSTRUCTION, so a position maps 1:1 between
        // hay, the masked line, and the original line. The old code matched
        // in byte space over `mline.to_lowercase()` and indexed char
        // buffers, which shifted every position after any non-ASCII
        // character and silently dropped (or mis-quoted) findings.
        let mchars: Vec<char> = mline.chars().collect();
        let hay: Vec<char> = mchars.iter().map(|c| c.to_ascii_lowercase()).collect();
        if rules.dictionary {
            for (s, e) in word_spans(mline) {
                // word_spans returns char spans; slice the char buffer.
                let tok: String = mchars[s..e].iter().collect();
                let key = tok.trim_matches('\'').to_ascii_lowercase();
                if let Some(entry) = FORMS.get(&key) {
                    let quote: String = ochars[s..e.min(ochars.len())].iter().collect();
                    findings.push(Finding {
                        line: lineno,
                        quote: clip(&quote),
                        rule: format!("{}-vocab", rules.prefix),
                        fix: format!(
                            "use {} (unapproved {} word: {key})",
                            repl_display(&entry.repl),
                            entry.pos
                        ),
                    });
                }
            }
        }
        if rules.latin {
            for (needle, fix) in LATIN {
                if let Some(pos) = find_word(&hay, needle) {
                    findings.push(Finding {
                        line: lineno,
                        quote: clip(&quote_at(&ochars, pos, needle)),
                        rule: format!("{}-latin", rules.prefix),
                        fix: (*fix).to_string(),
                    });
                }
            }
        }
        if rules.contractions {
            for (needle, fix) in CONTRACTIONS {
                if let Some(pos) = find_word(&hay, needle) {
                    findings.push(Finding {
                        line: lineno,
                        quote: clip(&quote_at(&ochars, pos, needle)),
                        rule: format!("{}-contraction", rules.prefix),
                        fix: (*fix).to_string(),
                    });
                }
            }
        }
        for w in rules.words {
            if let Some(pos) = find_word(&hay, w.needle) {
                findings.push(Finding {
                    line: lineno,
                    quote: clip(&quote_at(&ochars, pos, w.needle)),
                    rule: w.rule.to_string(),
                    fix: w.fix.to_string(),
                });
            }
        }
        if rules.semicolon && hay.contains(&';') {
            findings.push(Finding {
                line: lineno,
                quote: clip(oline),
                rule: format!("{}-semicolon", rules.prefix),
                fix: "write two sentences; the semicolon is not approved in STE".into(),
            });
        }
        if rules.exclamation && hay.contains(&'!') {
            findings.push(Finding {
                line: lineno,
                quote: clip(oline),
                rule: format!("{}-exclamation", rules.prefix),
                fix: "remove the exclamation mark and state the point plainly".into(),
            });
        }
    }

    if rules.length_caps {
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
                        rule: format!("{}-sentence-length", rules.prefix),
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
    }

    findings.sort_by_key(|f| (f.line, f.rule.clone(), f.quote.clone()));
    findings.dedup_by_key(|f| (f.line, f.rule.clone(), f.quote.clone()));
    findings
}

/// The original-line text under a needle found at char index `pos` of the
/// lowercased masked char buffer (positions map 1:1 back to the original
/// line: ASCII-only lowering and space-masking are length-preserving).
fn quote_at(ochars: &[char], pos: usize, needle: &str) -> String {
    let end = (pos + needle.chars().count()).min(ochars.len());
    ochars[pos..end].iter().collect()
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

    /// N4: a bias pair written with a SINGLE slash must reach the word
    /// rules; only tokens that look like paths get masked. The pre-fix
    /// predicate blanked every slash-containing token, so `master/slave`
    /// was invisible to the mechanical checker (a styles test pinned that
    /// blindness until 2026-10-02).
    #[test]
    fn slash_bias_word_reaches_rules_while_real_paths_stay_masked() {
        let text = "Open docs/style.css in the src/main tree, then fix the master/slave naming.";
        let m: String = mask(text).into_iter().collect();
        assert!(m.contains("master/slave"), "slash pair was masked: {m:?}");
        assert!(!m.contains("style.css"), "extension path survived: {m:?}");
        assert!(!m.contains("src/main"), "directory path survived: {m:?}");
        let f = check_with(text, &crate::styles::MICROSOFT_RULES);
        let bias: Vec<&str> = f
            .iter()
            .filter(|x| x.rule == "microsoft-bias")
            .map(|x| x.quote.as_str())
            .collect();
        assert_eq!(bias, ["master", "slave"], "{f:?}");
        assert!(
            f.iter().all(|x| {
                !x.quote.contains("css")
                    && !x.quote.contains("style")
                    && !x.quote.contains("src")
                    && !x.quote.contains("main")
            }),
            "path content leaked into findings: {f:?}"
        );
    }

    /// N4 companion: a single-slash token that is not a registered needle
    /// (a ratio, a date-shaped run) now reaches the rules unmasked and
    /// must still match nothing — no masking hid a rule hit that fires
    /// spuriously once the bare word passes through.
    #[test]
    fn single_slash_nonwords_match_no_rule() {
        let text = "Set the ratio to 12/17 on the release dated 2026-10-03 for 3/4 of the plan.";
        let m: String = mask(text).into_iter().collect();
        assert!(m.contains("12/17"), "ratio was masked: {m:?}");
        assert!(m.contains("3/4"), "fraction was masked: {m:?}");
        let f = check_with(text, &crate::styles::MICROSOFT_RULES);
        assert!(
            f.is_empty(),
            "spurious finding on a single-slash token: {f:?}"
        );
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
    /// The whole report is contractual for STE: rule ids, quotes, fixes,
    /// and the (line, rule, quote) emission order. A change to the engine's
    /// sort key, masking, or fix wording breaks this test even when the
    /// looser membership tests above stay green.
    #[test]
    fn ste_report_is_pinned_end_to_end() {
        let text = "Do not utilize the valve; it doesn't seal, e.g. when cold.\nCheck the seal, then initiate the pump.";
        let expected = vec![
            Finding {
                line: 1,
                quote: "doesn't".into(),
                rule: "ste-contraction".into(),
                fix: "does not".into(),
            },
            Finding {
                line: 1,
                quote: "e.g.".into(),
                rule: "ste-latin".into(),
                fix: "use for example".into(),
            },
            Finding {
                line: 1,
                quote: "Do not utilize the valve; it doesn't seal, e.g. when cold.".into(),
                rule: "ste-semicolon".into(),
                fix: "write two sentences; the semicolon is not approved in STE".into(),
            },
            Finding {
                line: 1,
                quote: "utilize".into(),
                rule: "ste-vocab".into(),
                fix: "use use (unapproved v word: utilize)".into(),
            },
            Finding {
                line: 2,
                quote: "Check".into(),
                rule: "ste-vocab".into(),
                fix: "use make / sure / measure / examine / check (unapproved v word: check)"
                    .into(),
            },
            Finding {
                line: 2,
                quote: "initiate".into(),
                rule: "ste-vocab".into(),
                fix: "use start (unapproved v word: initiate)".into(),
            },
            Finding {
                line: 2,
                quote: "pump".into(),
                rule: "ste-vocab".into(),
                fix: "use pump (unapproved v word: pump)".into(),
            },
        ];
        assert_eq!(check(text), expected);
    }

    // --- Non-ASCII finding correctness (M1 / m1 / m2 / N5 / N6) ------------
    //
    // These pin the char-space rewrite of the engine. The pre-fix engine
    // matched in byte space and indexed char buffers, so any line holding a
    // multibyte character before a token silently lost findings (M1),
    // mis-sliced quotes (m1), or mis-blanked the window after a URL (m2).

    /// M1: the same sentence must report the same dictionary violations
    /// whether or not the line also carries a non-ASCII character.
    #[test]
    fn non_ascii_line_still_reports_dictionary_words() {
        // ASCII premise: both tokens are in the dictionary.
        let ascii = check("The operator utilizes the gauge.");
        let quotes: Vec<&str> = ascii.iter().map(|x| x.quote.as_str()).collect();
        assert!(quotes.contains(&"utilizes"), "{quotes:?}");
        assert!(quotes.contains(&"gauge"), "{quotes:?}");
        // Same sentence, one em dash earlier in the line: the findings must
        // survive. Pre-fix this returned zero findings and the handler
        // reported clean:true.
        let dashed = check("The operator \u{2014} utilizes the gauge.");
        let quotes: Vec<&str> = dashed.iter().map(|x| x.quote.as_str()).collect();
        assert!(
            quotes.contains(&"utilizes"),
            "em dash lost the dictionary findings: {quotes:?}"
        );
        assert!(
            quotes.contains(&"gauge"),
            "em dash lost the dictionary findings: {quotes:?}"
        );
    }

    /// M1: tokens after a multibyte character must keep being reported,
    /// with their original spelling quoted verbatim.
    #[test]
    fn findings_survive_after_a_multibyte_char() {
        let f = check("Utilize the pump \u{2014} initiate the cycle, then utilize the seal.");
        let quotes: Vec<&str> = f
            .iter()
            .filter(|x| x.rule == "ste-vocab")
            .map(|x| x.quote.as_str())
            .collect();
        assert!(quotes.contains(&"Utilize"), "{quotes:?}");
        assert!(quotes.contains(&"initiate"), "{quotes:?}");
        assert!(
            quotes.contains(&"utilize"),
            "only the pre-dash tokens survived: {quotes:?}"
        );
    }

    /// m1: the needle position from the matcher indexes chars, not bytes,
    /// so the quote under a non-ASCII line is the needle itself.
    #[test]
    fn quote_at_indexes_non_ascii_lines_in_chars() {
        let f = check("Caf\u{e9}: check the seal, e.g. cold.");
        let latin: Vec<&str> = f
            .iter()
            .filter(|x| x.rule == "ste-latin")
            .map(|x| x.quote.as_str())
            .collect();
        assert_eq!(latin, vec!["e.g."], "{f:?}");
    }

    /// m2: the URL blanking window must be computed in char space over the
    /// masked buffer. On a line with a multibyte char before the URL, the
    /// pre-fix window (byte offsets of a rebuilt String applied as char
    /// indices) left the URL head visible and clipped the word after the
    /// URL, killing the `utilize` finding.
    #[test]
    fn url_masking_blanks_only_the_url_on_non_ascii_lines() {
        let text = "Le caf\u{e9}: see https://example.com/a/b then utilize the seal.";
        let m: String = mask(text).into_iter().collect();
        assert!(!m.contains("https"), "url head survived masking: {m:?}");
        assert!(!m.contains("example"), "url body survived masking: {m:?}");
        assert!(
            m.contains(" then utilize the seal."),
            "word after the url was clipped: {m:?}"
        );
        let f = check(text);
        assert!(
            f.iter()
                .any(|x| x.rule == "ste-vocab" && x.quote == "utilize"),
            "utilize after a url on a non-ascii line lost: {f:?}"
        );
    }

    /// N5: a sentence terminator counts when the character before it is
    /// alphanumeric in any script, not only ASCII.
    #[test]
    fn sentence_split_accepts_non_ascii_before_terminator() {
        // Discriminating fixture: the terminator follows 'e'-with-acute.
        // Pre-fix (ASCII-only prev-alnum) the whole two-sentence run merged
        // into a single "sentence".
        let para = vec![(
            1usize,
            "Le caf\u{e9} est ferm\u{e9}. The staff utilize the register.".to_string(),
        )];
        let sents: Vec<String> = collect_sentences(&para)
            .into_iter()
            .map(|(_, s)| s)
            .collect();
        assert_eq!(
            sents,
            vec![
                "Le caf\u{e9} est ferm\u{e9}.",
                "The staff utilize the register."
            ],
            "{sents:?}"
        );
        // The plain-ASCII neighbour keeps splitting as before.
        let para = vec![(
            1usize,
            "Caf\u{e9} is closed. The staff utilize the register.".to_string(),
        )];
        let sents: Vec<String> = collect_sentences(&para)
            .into_iter()
            .map(|(_, s)| s)
            .collect();
        assert_eq!(
            sents,
            vec!["Caf\u{e9} is closed.", "The staff utilize the register."],
            "{sents:?}"
        );
    }

    /// Hostile inputs must not panic. `check` is pure (no IO of any kind in
    /// this module), so purity holds by construction; these exercise the
    /// shapes the reviewer probed: CRLF, embedded NUL/ESC, lone curly
    /// quotes, an unclosed fence, empty text, a 100k-char single line.
    #[test]
    fn hostile_inputs_never_panic() {
        check("");
        check("\r\nThe operator utilizes\r\n the gauge.\r\n");
        check("A\u{0}B\u{1b}C utilize the seal.");
        check("The \u{201c}operator\u{201d} utilize the gauge.");
        // An unclosed fence exempts the rest of the document BY DESIGN,
        // matching CommonMark's treatment of an unterminated code fence.
        assert!(
            check("Unclosed fence\n```\nutilize initiate").is_empty(),
            "unclosed fence must exempt to EOF"
        );
        let big = "utilize ".repeat(12_500); // 100k chars, one line
        assert_eq!(big.chars().count(), 100_000);
        let f = check(&big);
        assert!(
            f.iter()
                .any(|x| x.rule == "ste-vocab" && x.quote == "utilize"),
            "100k-char line lost its findings"
        );
    }

    /// N6 perf smoke (NOT a benchmark): the old per-scheme loop rebuilt the
    /// haystack String per hit, which measured 440 ms for a 58 KB single
    /// line. The single left-to-right char pass must finish in well under a
    /// second — the bound is deliberately generous so CI never flakes.
    #[test]
    fn url_heavy_single_line_perf_smoke() {
        use std::time::Instant;
        let unit = "deploy http://host.example.invalid/a/b/c then configure the widget ";
        let line = unit.repeat(900); // ~59 KB single line, 900 http:// tokens
        let started = Instant::now();
        let findings = check(&line);
        let elapsed = started.elapsed();
        assert!(
            findings.iter().all(|x| !x.quote.contains("http")),
            "url content leaked into findings: {findings:?}"
        );
        assert!(
            elapsed.as_secs() < 1,
            "58 KB url-heavy single line took {elapsed:?} — masking regressed to quadratic"
        );
    }
}
