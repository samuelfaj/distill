// Modified for Distill by Samuel Fajreldines, 2026.
//! Terse output style for the agent's prose, adapted from the MIT-licensed caveman skill
//! (<https://github.com/juliusbrussee/caveman>). Cuts output tokens; code and exact strings stay intact.

/// How hard the agent compresses its prose. There is no off level: the style is always on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CavemanLevel {
    Lite,
    #[default]
    Full,
    Ultra,
}

impl CavemanLevel {
    /// Parses `lite|full|ultra` (case-insensitive, plus common "on" spellings). `None` for anything else, "off" spellings included.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "lite" | "light" => Some(Self::Lite),
            "full" | "on" | "true" | "1" | "yes" | "" => Some(Self::Full),
            "ultra" => Some(Self::Ultra),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lite => "lite",
            Self::Full => "full",
            Self::Ultra => "ultra",
        }
    }

    fn level_rule(self) -> &'static str {
        match self {
            Self::Lite => {
                "Level lite: drop filler and hedging, keep articles and full sentences. Professional but tight."
            }
            Self::Full => {
                "Level full: drop articles, fragments OK, short synonyms. Example: \"New object ref each render. Inline object prop = new ref = re-render. Wrap in `useMemo`.\""
            }
            Self::Ultra => {
                "Level ultra: also strip conjunctions when cause and effect stay unambiguous. One word when one word is enough. State each fact once. Example: \"Inline obj prop, new ref, re-render. `useMemo`.\""
            }
        }
    }

    /// The `<output_style>` body for this level.
    pub fn instructions(self) -> String {
        format!("{CAVEMAN_RULES}\n\n{}", self.level_rule())
    }
}

const CAVEMAN_RULES: &str = r#"Write prose terse like a smart caveman: all technical substance stays, only fluff dies. This style applies to every reply and every status line for the whole session.

- Drop filler (just/really/basically/actually/simply), pleasantries (sure/certainly/happy to), hedging and recaps. Pattern: `[thing] [action] [reason]. [next step].`
- No narration before or between tool calls: call tools directly. Write text before a call only to warn about security or an irreversible action, or to resolve ambiguity.
- No decorative tables or emoji. Quote only the shortest decisive line of an error log unless asked for more.
- Keep code, commands, paths, API names, identifiers, numbers, units and error strings exact. Never drop not/never/no/only/except.
- Do not invent abbreviations (cfg/impl/req/fn) or arrows; they save no tokens. Never add words to sound broken. If the terse phrasing is not shorter, use plain phrasing.
- Keep the user's language. One idea per sentence, active voice, same term for the same thing.
- Use full clear sentences for security warnings, irreversible-action confirmations, ordered multi-step instructions, and when the user asks for clarification; then resume.
- Files and messages persisted for other people stay in normal prose: code comments, commit messages, docs, PR/issue text, memory files, messages to third parties.
- This style is always on and cannot be turned off. If the user asks to stop it, keep it and say they can only pick lite, full or ultra."#;

#[cfg(test)]
mod tests {
    use super::CavemanLevel;

    #[test]
    fn parses_levels_and_on_spellings() {
        assert_eq!(CavemanLevel::parse("ULTRA"), Some(CavemanLevel::Ultra));
        assert_eq!(CavemanLevel::parse(" lite "), Some(CavemanLevel::Lite));
        assert_eq!(CavemanLevel::parse("on"), Some(CavemanLevel::Full));
        assert_eq!(CavemanLevel::parse("loud"), None);
    }

    /// The style is always on: no spelling may resolve to a level that renders nothing, or a
    /// stale `off` in a config file or a `/caveman off` would silence it for the whole session.
    #[test]
    fn no_spelling_turns_the_style_off() {
        for raw in [
            "off", "false", "0", "no", "none", "disable", "disabled", "normal",
        ] {
            assert_eq!(CavemanLevel::parse(raw), None, "{raw}");
        }
    }

    /// Every level must carry the exactness guard, because compression that mangles code or
    /// error strings breaks the work itself, and must say the style cannot be stopped.
    #[test]
    fn every_level_keeps_exactness_rule_and_cannot_be_stopped() {
        for level in [CavemanLevel::Lite, CavemanLevel::Full, CavemanLevel::Ultra] {
            let text = level.instructions();
            assert!(text.contains("error strings exact"), "{level:?}");
            assert!(text.contains("cannot be turned off"), "{level:?}");
            assert!(
                text.contains(&format!("Level {}", level.as_str())),
                "{level:?}"
            );
        }
    }
}
