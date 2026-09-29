// Modified for Distill by Samuel Fajreldines, 2026.
//! Terse output style for the agent's prose, adapted from the MIT-licensed caveman skill
//! (<https://github.com/juliusbrussee/caveman>). Cuts output tokens; code and exact strings stay intact.

/// How hard the agent compresses its prose. `Off` renders no `<output_style>` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CavemanLevel {
    Off,
    Lite,
    #[default]
    Full,
    Ultra,
}

impl CavemanLevel {
    /// Parses `off|lite|full|ultra` (case-insensitive, plus common on/off spellings). `None` for anything else.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "0" | "no" | "none" | "disable" | "disabled" | "normal" => {
                Some(Self::Off)
            }
            "lite" | "light" => Some(Self::Lite),
            "full" | "on" | "true" | "1" | "yes" | "" => Some(Self::Full),
            "ultra" => Some(Self::Ultra),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Lite => "lite",
            Self::Full => "full",
            Self::Ultra => "ultra",
        }
    }

    fn level_rule(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Lite => Some(
                "Level lite: drop filler and hedging, keep articles and full sentences. Professional but tight.",
            ),
            Self::Full => Some(
                "Level full: drop articles, fragments OK, short synonyms. Example: \"New object ref each render. Inline object prop = new ref = re-render. Wrap in `useMemo`.\"",
            ),
            Self::Ultra => Some(
                "Level ultra: also strip conjunctions when cause and effect stay unambiguous. One word when one word is enough. State each fact once. Example: \"Inline obj prop, new ref, re-render. `useMemo`.\"",
            ),
        }
    }

    /// The `<output_style>` body for this level, or `None` when off.
    pub fn instructions(self) -> Option<String> {
        self.level_rule()
            .map(|level| format!("{CAVEMAN_RULES}\n\n{level}"))
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
- The user can say "normal mode" or "stop caveman" to turn this off, or pick lite/full/ultra."#;

#[cfg(test)]
mod tests {
    use super::CavemanLevel;

    #[test]
    fn parses_levels_and_toggles() {
        assert_eq!(CavemanLevel::parse("ULTRA"), Some(CavemanLevel::Ultra));
        assert_eq!(CavemanLevel::parse(" lite "), Some(CavemanLevel::Lite));
        assert_eq!(CavemanLevel::parse("on"), Some(CavemanLevel::Full));
        assert_eq!(CavemanLevel::parse("off"), Some(CavemanLevel::Off));
        assert_eq!(CavemanLevel::parse("loud"), None);
    }

    /// Off must not cost prompt tokens; every other level must carry the exactness guard,
    /// because compression that mangles code or error strings breaks the work itself.
    #[test]
    fn off_renders_nothing_and_levels_keep_exactness_rule() {
        assert_eq!(CavemanLevel::Off.instructions(), None);
        for level in [CavemanLevel::Lite, CavemanLevel::Full, CavemanLevel::Ultra] {
            let text = level.instructions().unwrap();
            assert!(text.contains("error strings exact"), "{level:?}");
            assert!(
                text.contains(&format!("Level {}", level.as_str())),
                "{level:?}"
            );
        }
    }
}
