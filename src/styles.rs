//! The style registry — the extension point for external writing standards.
//!
//! A style is a name, a paraphrased (`&'static str`) prompt guide from
//! [`crate::prompts`] (never the guide's own text — the source guides are
//! copyrighted), the head line that names the standard to the model, the
//! critique-side judging sentence, and an optional mechanical rule set for
//! [`crate::writer::style_check`] (the deterministic half a small model must
//! not be trusted with).
//!
//! Adding a style is four steps:
//! 1. Paraphrase its rules into a `*_GUIDE` const in `prompts.rs` — a summary,
//!    never an excerpt.
//! 2. If it has mechanically checkable rules, define them as a [`Rules`]
//!    const here ([`GOOGLE_RULES`] and [`MICROSOFT_RULES`] are the models);
//!    otherwise set `rules: None`.
//! 3. Add one row to [`STYLES`] below.
//! 4. `cargo test` must stay green — the registry invariants are pinned — and
//!    the new guide needs a live model call before it ships (see AGENTS.md).
//!
//! One style per call. Styles are prose or structure standards; combining two
//! selected styles is not supported, and resolving conflicting rules is a
//! later feature — the registry deliberately does not define a merge order.

use crate::prompts;
use crate::ste::{self, Rules, WordRule};

/// One selectable writing standard.
#[derive(Debug)]
pub struct Style {
    /// The argument value: `style: "ste"`. Lookups are case-insensitive.
    pub id: &'static str,
    /// Human-facing name, quoted to the model and in refusals.
    pub name: &'static str,
    /// Prompt block appended to the base house guide when a call selects
    /// this style.
    pub guide: &'static str,
    /// Line naming the standard in the user-message head (draft, rewrite,
    /// compose).
    pub head_line: &'static str,
    /// Sentence appended to the critique head: what to judge, and how to
    /// report it.
    pub critique_line: &'static str,
    /// The deterministic half, when the standard has mechanically checkable
    /// rules. `None` styles are refused by `style_check` with a clear
    /// message — an empty findings list must never look like a pass.
    pub rules: Option<&'static Rules>,
}

/// The value that opts out of every external standard (house style only),
/// used by the server default and the per-call argument alike.
pub const NONE_ID: &str = "none";

/// Google developer documentation style guide, mechanical half: the
/// mechanical primitives Google states, plus its tone- and slang-word
/// bans. Deliberately narrow — heading capitalisation, the serial comma,
/// and tense judgment are prompt-side only, where the model can weigh
/// context a word-boundary matcher cannot.
pub static GOOGLE_RULES: Rules = Rules {
    prefix: "google",
    dictionary: false,
    latin: true,
    contractions: false,
    semicolon: false,
    exclamation: true,
    length_caps: false,
    words: &[
        WordRule {
            needle: "simply",
            rule: "google-tone",
            fix: "never call a step easy; delete simply",
        },
        WordRule {
            needle: "easily",
            rule: "google-tone",
            fix: "delete easily; nothing is easy to a reader in a hurry",
        },
        WordRule {
            needle: "just",
            rule: "google-tone",
            fix: "delete just, or name the step directly",
        },
        WordRule {
            needle: "obviously",
            rule: "google-tone",
            fix: "delete obviously; nothing is obvious to a reader in a hurry",
        },
        WordRule {
            needle: "of course",
            rule: "google-tone",
            fix: "delete of course; nothing is a given to a new reader",
        },
        WordRule {
            needle: "please",
            rule: "google-please",
            fix: "delete please; instructions do not say please",
        },
        WordRule {
            needle: "let's",
            rule: "google-lets",
            fix: "address the reader as you; do not write let's",
        },
        WordRule {
            needle: "tl;dr",
            rule: "google-internet-slang",
            fix: "write a plain summary; tl;dr is internet slang",
        },
        WordRule {
            needle: "ymmv",
            rule: "google-internet-slang",
            fix: "write the caveat in plain words; ymmv is internet slang",
        },
        WordRule {
            needle: "rtfm",
            rule: "google-internet-slang",
            fix: "point to the documentation; rtfm is not an answer",
        },
    ],
};

/// Microsoft Writing Style Guide, mechanical half: the mechanical primitives
/// the guide states, plus the bias-free terms it lists by name. Contractions
/// are Microsoft house style, so the checker must never flag them; sentence
/// length is prompt-side judgment, not a published cap.
pub static MICROSOFT_RULES: Rules = Rules {
    prefix: "microsoft",
    dictionary: false,
    latin: true,
    contractions: false,
    semicolon: false,
    exclamation: true,
    length_caps: false,
    words: &[
        WordRule {
            needle: "please",
            rule: "microsoft-please",
            fix: "delete please; instructions and UI text do not say please",
        },
        WordRule {
            needle: "manpower",
            rule: "microsoft-bias",
            fix: "use workforce or staff",
        },
        WordRule {
            needle: "mankind",
            rule: "microsoft-bias",
            fix: "use humankind or people",
        },
        WordRule {
            needle: "chairman",
            rule: "microsoft-bias",
            fix: "use chair or moderator",
        },
        WordRule {
            needle: "salesman",
            rule: "microsoft-bias",
            fix: "use sales representative",
        },
        WordRule {
            needle: "manmade",
            rule: "microsoft-bias",
            fix: "use synthetic or manufactured",
        },
        WordRule {
            needle: "master",
            rule: "microsoft-bias",
            fix: "where paired with slave, use primary and subordinate; otherwise name the role",
        },
        WordRule {
            needle: "slave",
            rule: "microsoft-bias",
            fix: "use replica, standby, or subordinate",
        },
        WordRule {
            needle: "blacklist",
            rule: "microsoft-bias",
            fix: "use block list",
        },
        WordRule {
            needle: "whitelist",
            rule: "microsoft-bias",
            fix: "use allow list",
        },
        WordRule {
            needle: "guys",
            rule: "microsoft-bias",
            fix: "address a group as everyone or you",
        },
        WordRule {
            needle: "hang",
            rule: "microsoft-bias",
            fix: "say stops responding; hang is slang",
        },
        WordRule {
            needle: "hangs",
            rule: "microsoft-bias",
            fix: "say stops responding; hangs is slang",
        },
    ],
};

/// The registry. One row per external standard; order is display order in
/// refusals and usage strings only — no style is privileged.
static STYLES: &[&Style] = &[
    &Style {
        id: "ste",
        name: "ASD-STE100 Simplified Technical English (issue 8)",
        guide: prompts::STE_GUIDE,
        head_line: "Standard: ASD-STE100 Simplified Technical English (issue 8).",
        critique_line: " Judge ASD-STE100 Simplified Technical English faults too \
            (unapproved vocabulary, semicolons, contractions, sentences over 20 words \
            for an instruction or 25 for description), quoting the phrase and giving \
            the approved fix.",
        rules: Some(&ste::STE_RULES),
    },
    &Style {
        id: "google",
        name: "Google developer documentation style guide",
        guide: prompts::GOOGLE_GUIDE,
        head_line: "Standard: Google developer documentation style guide.",
        critique_line: " Judge Google developer documentation style faults too \
            (please in instructions, simply/easily/just/obviously tone words, internet \
            abbreviations, Latin abbreviations, future tense for general behavior, \
            exclamation marks, title-case or punctuated headings, missing serial \
            comma), quoting the phrase and giving the Google-style fix.",
        rules: Some(&GOOGLE_RULES),
    },
    &Style {
        id: "microsoft",
        name: "Microsoft Writing Style Guide",
        guide: prompts::MICROSOFT_GUIDE,
        head_line: "Standard: Microsoft Writing Style Guide.",
        critique_line: " Judge Microsoft Writing Style Guide faults too (missing \
            contractions where the voice should read human, you can and there is weak \
            openings, passive voice that obscures the actor, please, exclamation marks, \
            title-case or punctuated headings, gendered generic pronouns, the \
            bias-free term list, Latin abbreviations), quoting the phrase and giving \
            the Microsoft-style fix.",
        rules: Some(&MICROSOFT_RULES),
    },
    &Style {
        id: "diataxis",
        name: "Diátaxis documentation architecture",
        guide: prompts::DIATAXIS_GUIDE,
        head_line: "Structure: Diátaxis — one document, one mode: tutorial, how-to, reference, or explanation.",
        critique_line: " Judge the text as Diátaxis too: name the mode it aims for, \
            flag mixed modes (a tutorial that explains, a reference that instructs, a \
            how-to that teaches or promotes), and quote the drift, saying which content \
            belongs in which mode.",
        rules: None,
    },
];

/// Look a style up by argument value, case-insensitive and trim-tolerant.
/// `none` and unknown ids return `None`: the caller refuses them with the
/// valid list, exactly like unknown `kind` values.
pub fn lookup(id: &str) -> Option<&'static Style> {
    let id = id.trim();
    STYLES
        .iter()
        .copied()
        .find(|s| s.id.eq_ignore_ascii_case(id))
}

/// Every style id plus `none`, for usage strings and refusals.
pub fn usage() -> String {
    let mut ids: Vec<&str> = STYLES.iter().map(|s| s.id).collect();
    ids.insert(0, NONE_ID);
    ids.join(", ")
}

/// The ids that have a deterministic checker, for `style_check` refusals.
pub fn checkable() -> String {
    STYLES
        .iter()
        .filter(|s| s.rules.is_some())
        .map(|s| s.id)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        for (i, a) in STYLES.iter().enumerate() {
            for b in &STYLES[i + 1..] {
                assert_ne!(a.id, b.id);
                assert_ne!(a.rules.map(|r| r.prefix), b.rules.map(|r| r.prefix));
            }
        }
    }

    #[test]
    fn lookup_is_case_insensitive_and_trim_tolerant() {
        assert_eq!(lookup("Google").unwrap().id, "google");
        assert_eq!(lookup("  MICROSOFT ").unwrap().id, "microsoft");
        assert_eq!(lookup("ste").unwrap().id, "ste");
        assert_eq!(lookup("Diataxis").unwrap().id, "diataxis");
    }

    #[test]
    fn none_and_unknown_ids_look_up_as_none() {
        assert!(lookup("none").is_none());
        assert!(lookup("NONE").is_none());
        assert!(lookup("chicago").is_none());
        assert!(lookup("").is_none());
    }

    #[test]
    fn usage_and_checkable_lists() {
        assert_eq!(usage(), "none, ste, google, microsoft, diataxis");
        assert_eq!(checkable(), "ste, google, microsoft");
    }

    #[test]
    fn diataxis_carries_a_guide_but_no_mechanical_rules() {
        let s = lookup("diataxis").unwrap();
        assert!(s.rules.is_none());
        assert!(s.guide.contains("Diátaxis mode"));
    }

    #[test]
    fn google_rules_detect_latin_tone_and_slang() {
        let text = "Deploy the app, e.g. on Fridays. Simply configure it, then start. \
            Read the manual (rtfm) for details!";
        let rules: Vec<String> = ste::check_with(text, &GOOGLE_RULES)
            .into_iter()
            .map(|f| f.rule)
            .collect();
        assert!(rules.contains(&"google-latin".to_string()), "{rules:?}");
        assert!(rules.contains(&"google-tone".to_string()), "{rules:?}");
        assert!(
            rules.contains(&"google-internet-slang".to_string()),
            "{rules:?}"
        );
        assert!(
            rules.contains(&"google-exclamation".to_string()),
            "{rules:?}"
        );
    }

    #[test]
    fn google_rules_ignore_ste_only_mechanics() {
        // A semicolon and a contraction are STE's rules, not Google's: the
        // Google checker must stay silent on them.
        let text = "You can't skip the step; the retry needs a full cycle.";
        let findings = ste::check_with(text, &GOOGLE_RULES);
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn google_rules_flag_please_and_lets() {
        let text = "Please run the migration. Let's begin with the schema.";
        let rules: Vec<String> = ste::check_with(text, &GOOGLE_RULES)
            .into_iter()
            .map(|f| f.rule)
            .collect();
        assert!(rules.contains(&"google-please".to_string()), "{rules:?}");
        assert!(rules.contains(&"google-lets".to_string()), "{rules:?}");
    }

    #[test]
    fn microsoft_rules_detect_bias_terms_exclamation_and_latin() {
        let text = "The manpower must whitelist the legacy endpoint; he/she can retry. \
            The server hangs sometimes. Great job!";
        let rules: Vec<String> = ste::check_with(text, &MICROSOFT_RULES)
            .into_iter()
            .map(|f| f.rule)
            .collect();
        assert!(rules.contains(&"microsoft-bias".to_string()), "{rules:?}");
        assert!(
            rules.contains(&"microsoft-exclamation".to_string()),
            "{rules:?}"
        );
    }

    #[test]
    fn microsoft_rules_never_flag_contractions() {
        // Contractions are Microsoft house style; a checker that flagged
        // them would fight the guide it serves.
        let text = "If you've enabled it, you'll see the result — it's fine.";
        let findings = ste::check_with(text, &MICROSOFT_RULES);
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn microsoft_rules_flag_latin_abbreviations() {
        let text = "Support formats, i.e. PNG and WebP.";
        let rules: Vec<String> = ste::check_with(text, &MICROSOFT_RULES)
            .into_iter()
            .map(|f| f.rule)
            .collect();
        assert!(rules.contains(&"microsoft-latin".to_string()), "{rules:?}");
    }

    #[test]
    fn code_and_urls_are_exempt_under_extra_word_rules() {
        // `simply` inside inline code and a URL path must not be found.
        let text = "Run `simply deploy` at https://example.com/just/deploy and note the whitelist.";
        let rules: Vec<String> = ste::check_with(text, &GOOGLE_RULES)
            .into_iter()
            .map(|f| f.rule)
            .collect();
        assert!(!rules.contains(&"google-tone".to_string()), "{rules:?}");
        assert!(
            rules.contains(&"google-internet-slang".to_string())
                || !rules.iter().any(|r| r.starts_with("google-")),
            "only prose rules may fire: {rules:?}"
        );
    }

    /// The `master` and `slave` needles each fire once on the space-,
    /// hyphen-, and slash-joined pair forms. The compound rows they once
    /// duplicated only tripled reports or could never fire. The slash
    /// spelling must reach the word rules: before 2026-10-02 the path
    /// masker blanked every slash-containing token, and this test pinned
    /// that blindness (`is_empty()`); bias words written with a slash were
    /// invisible to the mechanical checker.
    #[test]
    fn bias_pair_reports_once_per_word() {
        for join in ["master slave", "master-slave", "master/slave"] {
            let quotes: Vec<String> =
                ste::check_with(&format!("Configure the {join} pair."), &MICROSOFT_RULES)
                    .into_iter()
                    .filter(|f| f.rule == "microsoft-bias")
                    .map(|f| f.quote)
                    .collect();
            assert_eq!(quotes, ["master", "slave"], "{join}");
        }
    }
}
