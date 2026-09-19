// Modified for Distill by Samuel Fajreldines, 2026.
//! Environment variable expansion helper for hook config strings.
//!
//! Expands `${VAR}` / `$VAR`, consulting a per-hook `extra_env` map before the process environment.
//! [`crate::config::parse_hook_file`] uses it on `command` and `url` fields at config-load time.
//! [`crate::runner::http`] uses it on `spec.url` once more right before SSRF validation.
//!
//! Unset plain references (e.g. `${UNSET}/x`) are preserved verbatim.
//! So is every parameter-expansion-modifier form (`:-`, `-`, `:=`, `:?`, `:+`, `%`, `#`, `/`, `:N`, `:N:M`, e.g. `${VAR:-default}`).
//! Preserving them keeps config-load-time expansion idempotent: re-running it on an already expanded string is a no-op.
//! Vars deferred to runtime (set later by the shell, the dispatcher, or `extra_env`) survive the load-time pass.
//! The pre-flight check in [`crate::runner::command`] catches any that remain unset at execution.
//! Modifier forms have shell-specific behaviour (notably `:-` on a set-but-empty value differs between `sh` and shellexpand).
//! The user wrote the modifier form because they wanted the shell's interpretation, so the runtime `sh -c` branch resolves it.
//! [`crate::runner::command::find_unresolved_env_vars`] mirrors the modifier-skip behaviour, keeping the two layers in sync.
//!
//! The engine is `shellexpand::env_with_context_no_errors`, shared with `distill_config::expand_env_vars_in_string`.
//! This version adds the per-hook `extra` map and the modifier-form preservation.
//!
//! ## Asymmetry between `command` and `url`
//!
//! Load-time expansion in [`crate::config::parse_hook_file`] runs once, using a snapshot of process env at parse time.
//! The HTTP runner does a second pass at runtime so plugin-injected vars arriving in `extra_env` after parsing (e.g. `CLAUDE_PLUGIN_ROOT`) resolve.
//! The second pass also picks up mid-session changes to process env for URLs.
//! Command paths are not value-expanded at spawn.
//! Unix `sh -c` expands `$VAR` from the child env; Windows PowerShell rewrites known `$VAR` to `$env:VAR`.

use std::collections::HashMap;

/// Sentinel prefix for the per-call mask sentinel; see [`make_sentinel`].
///
/// A Unicode Private Use Area code point (`U+F8FF`) plus a long magic ASCII prefix.
const SENTINEL_PREFIX: &str = "\u{f8ff}__GROK_HOOKS_MASK_";
const SENTINEL_SUFFIX: &str = "__\u{f8ff}";

/// Build the per-call sentinel that hides modifier-form `${...}` substrings from `shellexpand::env_with_context_no_errors`.
/// The sentinel is restored to `${` after shellexpand runs, so the modifier form survives expansion verbatim.
/// With per-call randomization the chance of a natural collision is ~2^-128.
fn make_sentinel() -> String {
    let hi: u64 = fastrand::u64(..);
    let lo: u64 = fastrand::u64(..);
    format!("{SENTINEL_PREFIX}{hi:016x}{lo:016x}{SENTINEL_SUFFIX}")
}

/// Expand `${VAR}` / `$VAR` references in `input`.
/// Unresolved references are preserved verbatim, so the function is idempotent on already-expanded strings.
/// References resolved only at runtime survive the load-time pass.
pub(crate) fn expand_env_vars_with_extra(input: &str, extra: &HashMap<String, String>) -> String {
    expand_env_vars_with_process_skip(input, extra, &[])
}

pub(crate) fn expand_env_vars_with_process_skip(
    input: &str,
    extra: &HashMap<String, String>,
    skip_process_env: &[&str],
) -> String {
    let sentinel = make_sentinel();

    // Defence in depth: a collision between the fresh sentinel and the input or an extra-env value would require predicting our PRNG output
    // Panic in debug builds and fall through to legacy behaviour in release
    // Returning the input unchanged is safer than rewriting a legitimate substring to `${`
    debug_assert!(
        !input.contains(&sentinel) && !extra.values().any(|v| v.contains(&sentinel)),
        "per-call sentinel collided with input or extra-env value"
    );

    // Step 1: hide any `${VAR<modifier>...}` substring from shellexpand by replacing the leading `${` with the per-call sentinel
    // shellexpand needs a `$` before the brace to recognize the form, so the masked body reads as literal text
    let masked = mask_modifier_forms(input, &sentinel);

    // Step 2: run shellexpand on the (possibly) masked input.
    let context = |name: &str| -> Option<String> {
        if let Some(v) = extra.get(name) {
            return Some(v.clone());
        }
        if skip_process_env.contains(&name) {
            return None;
        }
        std::env::var(name).ok()
    };
    let expanded = shellexpand::env_with_context_no_errors(&masked, context).into_owned();

    // Step 3: restore the sentinels back to `${`
    // The sentinel is freshly randomized per call, so it appears in `expanded` only where `mask_modifier_forms` put it
    if expanded.contains(&sentinel) {
        expanded.replace(&sentinel, "${")
    } else {
        expanded
    }
}

/// Replace the leading `${` of every modifier-form `${...}` substring (valid identifier plus a parameter-expansion modifier) with `sentinel`.
/// Plain `${VAR}` and bare `$VAR` references are NOT touched; they pass through to shellexpand for normal resolution.
/// "Modifier" means anything inside the braces after the identifier: `:-`, `-`, `:=`, `=`, `:?`, `?`, `:+`, `+`, `%`, `#`, `/`, `:N`, `:N:M`, etc.
fn mask_modifier_forms(input: &str, sentinel: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut cursor: usize = 0;
    for r in iter_env_var_references(input) {
        // Copy any literal text between the previous reference (or start of string) and this one verbatim
        if cursor < r.start
            && let Some(lit) = input.get(cursor..r.start)
        {
            out.push_str(lit);
        }
        // Modifier-form braced ref: replace leading `${` with sentinel and emit the body (including closing `}`) as-is
        if r.braced && r.has_modifier {
            out.push_str(sentinel);
            if let Some(body) = input.get(r.start + 2..r.end) {
                out.push_str(body);
            }
        } else if let Some(raw) = input.get(r.start..r.end) {
            // Plain `${NAME}`, bare `$NAME`, or invalid form: pass through verbatim so shellexpand can resolve (or leave unresolved)
            out.push_str(raw);
        }
        cursor = r.end;
    }
    // Copy the trailing literal tail.
    if cursor < input.len()
        && let Some(tail) = input.get(cursor..)
    {
        out.push_str(tail);
    }
    out
}

/// One detected env-var reference in a string, as produced by [`iter_env_var_references`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnvVarRef<'a> {
    /// Byte offset where the leading `$` starts.
    pub start: usize,
    /// Byte offset one past the end of the reference.
    /// For braced forms this is one past the closing `}`; for bare forms it is one past the last identifier character.
    pub end: usize,
    /// Identifier name.
    /// For `${VAR...}` and `$VAR` this is `"VAR"`; for invalid forms (e.g. `${:-foo}`, `${}`) it is empty.
    pub name: &'a str,
    /// True for `${...}` (braced); false for `$NAME` (bare).
    pub braced: bool,
    /// True if the braced form has a modifier (`:`, `-`, `=`, `?`, `+`, `%`, `#`, `/`, digit suffix, etc.) between the identifier and closing `}`.
    /// Always false for bare references and for invalid braced forms.
    pub has_modifier: bool,
}

/// Walk `input` and yield every `$VAR` / `${...}` reference.
/// Skips shell positional / special params (`$1`, `$$`, `$?`, `$#`, `$(...)`, `$@`, etc.) since none of those are env-var references.
/// Unterminated braced forms (`${VAR:-no-close`) are skipped: the `$` is consumed and scanning continues at the next byte. This matches `shellexpand`, which treats unterminated forms as literal text; Nested braces inside a modifier body (`${A:-${B}}`) match the FIRST `}`, so the inner `${B}` becomes part of the outer modifier body. The runtime `sh -c` branch handles real nesting natively when the form reaches the shell; Empty / invalid identifier (`${}`, `${:-foo}`) is yielded with an empty `name`, so callers can decide whether to mask it.
pub(crate) fn iter_env_var_references(input: &str) -> EnvVarRefIter<'_> {
    EnvVarRefIter { input, pos: 0 }
}

pub(crate) struct EnvVarRefIter<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Iterator for EnvVarRefIter<'a> {
    type Item = EnvVarRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let bytes = self.input.as_bytes();
        while self.pos < bytes.len() {
            if bytes.get(self.pos).copied() != Some(b'$') {
                self.pos += 1;
                continue;
            }
            let dollar = self.pos;
            let after = dollar + 1;
            if after >= bytes.len() {
                // Trailing lone `$` is not a reference. Stop.
                self.pos = bytes.len();
                return None;
            }
            if bytes.get(after).copied() == Some(b'{') {
                // Braced form: ${...}
                let body_start = after + 1;
                // Read identifier prefix (alphanumeric / underscore).
                let mut name_end = body_start;
                while bytes
                    .get(name_end)
                    .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
                {
                    name_end += 1;
                }
                // Find the FIRST closing `}` from the identifier end.
                let mut close = name_end;
                while bytes.get(close).is_some_and(|&b| b != b'}') {
                    close += 1;
                }
                if close >= bytes.len() {
                    // Unterminated brace is not a real form. Skip the `$` and keep scanning.
                    self.pos = dollar + 1;
                    continue;
                }
                let name = bytes
                    .get(body_start..name_end)
                    .and_then(|s| std::str::from_utf8(s).ok())
                    .unwrap_or("");
                let has_modifier = !name.is_empty() && name_end < close;
                let end = close + 1;
                self.pos = end;
                return Some(EnvVarRef {
                    start: dollar,
                    end,
                    name,
                    braced: true,
                    has_modifier,
                });
            }
            // Bare `$NAME`: identifier must start with letter / `_`.
            // Anything else (`$1`, `$$`, `$?`, `$#`, `$(`, etc.) is a shell special and not an env-var reference
            if bytes
                .get(after)
                .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
            {
                let start_id = after;
                let mut end_id = start_id;
                while bytes
                    .get(end_id)
                    .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
                {
                    end_id += 1;
                }
                let name = bytes
                    .get(start_id..end_id)
                    .and_then(|s| std::str::from_utf8(s).ok())
                    .unwrap_or("");
                self.pos = end_id;
                return Some(EnvVarRef {
                    start: dollar,
                    end: end_id,
                    name,
                    braced: false,
                    has_modifier: false,
                });
            }
            // `$` followed by a non-identifier, non-`{` byte. Skip both bytes and continue.
            self.pos = after + 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_env_var;

    #[test]
    fn expands_braced_var_from_extra() {
        let mut extra = HashMap::new();
        extra.insert("PLUGIN_HOST".to_string(), "example.com".to_string());
        let out = expand_env_vars_with_extra("https://${PLUGIN_HOST}/check", &extra);
        assert_eq!(out, "https://example.com/check");
    }

    #[test]
    fn expands_bare_var_from_extra() {
        let mut extra = HashMap::new();
        extra.insert("ROOT".to_string(), "/opt/plugin".to_string());
        let out = expand_env_vars_with_extra("$ROOT/bin/x.sh", &extra);
        assert_eq!(out, "/opt/plugin/bin/x.sh");
    }

    #[test]
    fn extra_takes_precedence_over_process_env() {
        with_env_var(
            "GROK_HOOKS_ENV_EXPAND_TEST_PRECEDENCE",
            Some("from-process"),
            || {
                let mut extra = HashMap::new();
                extra.insert(
                    "GROK_HOOKS_ENV_EXPAND_TEST_PRECEDENCE".to_string(),
                    "from-extra".to_string(),
                );
                let out =
                    expand_env_vars_with_extra("${GROK_HOOKS_ENV_EXPAND_TEST_PRECEDENCE}", &extra);
                assert_eq!(out, "from-extra");
            },
        );
    }

    #[test]
    fn falls_back_to_process_env() {
        with_env_var(
            "GROK_HOOKS_ENV_EXPAND_TEST_FALLBACK",
            Some("/from/proc/env"),
            || {
                let extra = HashMap::new();
                let out =
                    expand_env_vars_with_extra("${GROK_HOOKS_ENV_EXPAND_TEST_FALLBACK}/x", &extra);
                assert_eq!(out, "/from/proc/env/x");
            },
        );
    }

    #[test]
    fn preserves_unresolved_references() {
        // shellexpand's no-errors variant returns the original `${VAR}` text when the var is unset in both `extra` and the process env
        // This makes load-time expansion idempotent and lets runtime-only vars survive the pass to be caught by `find_unresolved_env_vars`
        with_env_var("GROK_HOOKS_ENV_EXPAND_NEVER_SET", None, || {
            let extra = HashMap::new();
            let input = "${GROK_HOOKS_ENV_EXPAND_NEVER_SET}/x.sh";
            let out = expand_env_vars_with_extra(input, &extra);
            assert_eq!(out, input);
        });
    }

    #[test]
    fn process_skip_leaves_runner_names() {
        assert_eq!(
            crate::config::expand_env_skipping_runner_vars("${CLAUDE_PROJECT_DIR:-.}"),
            "${CLAUDE_PROJECT_DIR:-.}"
        );
    }

    #[test]
    fn idempotent_on_already_expanded_string() {
        let extra = HashMap::new();
        let already = "/opt/plugins/foo/hooks/x.sh";
        let out = expand_env_vars_with_extra(already, &extra);
        assert_eq!(out, already);
    }

    #[test]
    fn empty_input_returns_empty() {
        let extra = HashMap::new();
        assert_eq!(expand_env_vars_with_extra("", &extra), "");
    }

    // ── Parameter-expansion-modifier preservation ───────────────

    /// `${VAR:-default}` must be preserved verbatim, even when `VAR` is unset at expand time.
    /// Otherwise shellexpand resolves to the literal default and the runtime branch never sees `VAR`'s real (runtime-only) value.
    #[test]
    fn preserves_default_modifier_when_var_unset() {
        let extra = HashMap::new();
        with_env_var("GROK_HOOKS_ENV_EXPAND_MODIFIER_UNSET", None, || {
            let input = "${GROK_HOOKS_ENV_EXPAND_MODIFIER_UNSET:-/default/path.sh}";
            let out = expand_env_vars_with_extra(input, &extra);
            assert_eq!(out, input);
        });
    }

    /// Even when the var IS set, the modifier form must be preserved verbatim.
    /// The shell's `:-` semantics differ from shellexpand's, notably for set-but-empty values, so the whole form is left for the shell.
    #[test]
    fn preserves_default_modifier_when_var_set() {
        let mut extra = HashMap::new();
        extra.insert(
            "GROK_HOOKS_DEFAULT_SET".to_string(),
            "/from/extra".to_string(),
        );
        let input = "${GROK_HOOKS_DEFAULT_SET:-/fallback}";
        let out = expand_env_vars_with_extra(input, &extra);
        assert_eq!(out, input);
    }

    #[test]
    fn preserves_no_colon_default_modifier() {
        let extra = HashMap::new();
        let input = "${GROK_HOOKS_NCD-/fallback}";
        let out = expand_env_vars_with_extra(input, &extra);
        assert_eq!(out, input);
    }

    #[test]
    fn preserves_assignment_modifier() {
        let extra = HashMap::new();
        let input = "${GROK_HOOKS_ASSIGN:=/assigned/path.sh}";
        let out = expand_env_vars_with_extra(input, &extra);
        assert_eq!(out, input);
    }

    #[test]
    fn preserves_error_modifier() {
        let extra = HashMap::new();
        let input = "${GROK_HOOKS_ERR:?error message}";
        let out = expand_env_vars_with_extra(input, &extra);
        assert_eq!(out, input);
    }

    #[test]
    fn preserves_alternate_modifier() {
        let extra = HashMap::new();
        let input = "${GROK_HOOKS_ALT:+/used/if/set}";
        let out = expand_env_vars_with_extra(input, &extra);
        assert_eq!(out, input);
    }

    #[test]
    fn preserves_suffix_strip_modifier() {
        let extra = HashMap::new();
        let input = "${GROK_HOOKS_SUFFIX%.sh}";
        let out = expand_env_vars_with_extra(input, &extra);
        assert_eq!(out, input);
    }

    #[test]
    fn preserves_prefix_strip_modifier() {
        let extra = HashMap::new();
        let input = "${GROK_HOOKS_PREFIX#prefix/}";
        let out = expand_env_vars_with_extra(input, &extra);
        assert_eq!(out, input);
    }

    #[test]
    fn preserves_substitution_modifier() {
        let extra = HashMap::new();
        let input = "${GROK_HOOKS_SUB/foo/bar}";
        let out = expand_env_vars_with_extra(input, &extra);
        assert_eq!(out, input);
    }

    #[test]
    fn preserves_substring_modifier() {
        let extra = HashMap::new();
        let input = "${GROK_HOOKS_SUBSTR:0:5}";
        let out = expand_env_vars_with_extra(input, &extra);
        assert_eq!(out, input);
    }

    /// Mixed: a modifier-form sits next to a plain form; only the plain one is expanded.
    #[test]
    fn mixed_plain_and_modifier_only_plain_expanded() {
        let mut extra = HashMap::new();
        extra.insert("GROK_HOOKS_PLAIN".to_string(), "/usr/local".to_string());
        let input = "${GROK_HOOKS_PLAIN}/${GROK_HOOKS_DEFER:-/fallback}";
        let out = expand_env_vars_with_extra(input, &extra);
        assert_eq!(out, "/usr/local/${GROK_HOOKS_DEFER:-/fallback}");
    }

    // ── Set-but-empty regression test ────────────────────────────

    /// When the var is set in `extra` but to the empty string, the no-modifier form `${VAR}` resolves to "", matching shellexpand.
    #[test]
    fn empty_extra_value_resolves_to_empty_for_plain_form() {
        let mut extra = HashMap::new();
        extra.insert("GROK_HOOKS_EMPTY".to_string(), "".to_string());
        let out = expand_env_vars_with_extra("[${GROK_HOOKS_EMPTY}]", &extra);
        assert_eq!(out, "[]");
    }

    /// When the var is set in `extra` but to the empty string, the modifier form `${VAR:-default}` is preserved verbatim.
    /// The runtime `sh -c` branch applies POSIX `:-` semantics: bash returns the default for an empty value, shellexpand the empty string.
    #[test]
    fn empty_extra_value_does_not_trigger_default() {
        let mut extra = HashMap::new();
        extra.insert("GROK_HOOKS_EMPTY_MOD".to_string(), "".to_string());
        let input = "${GROK_HOOKS_EMPTY_MOD:-/fallback}";
        let out = expand_env_vars_with_extra(input, &extra);
        assert_eq!(out, input);
    }

    // ── Single-pass expansion (no recursion) ────────────────────

    /// A value in `extra` that itself contains a `$VAR` reference must NOT be re-expanded.
    /// Recursion would be a DoS vector and a semantic surprise.
    /// shellexpand's `env_with_context_no_errors` is single-pass by design; this test locks the property in.
    #[test]
    fn extra_values_are_not_recursively_expanded() {
        with_env_var(
            "GROK_HOOKS_RECURSION_BAR",
            Some("should-not-appear"),
            || {
                let mut extra = HashMap::new();
                extra.insert(
                    "GROK_HOOKS_RECURSION_FOO".to_string(),
                    "$GROK_HOOKS_RECURSION_BAR".to_string(),
                );
                let out = expand_env_vars_with_extra("${GROK_HOOKS_RECURSION_FOO}", &extra);
                assert_eq!(out, "$GROK_HOOKS_RECURSION_BAR");
            },
        );
    }

    // ── mask_modifier_forms helper unit tests ────────────────────

    /// A fixed test-only sentinel that makes the masked-output assertions deterministic.
    /// Production code uses the per-call randomized [`make_sentinel`]; the sentinel collision regression tests below exercise the random path.
    const TEST_SENTINEL: &str = "<<TEST_SENTINEL>>";

    #[test]
    fn mask_helper_passes_plain_form_through() {
        assert_eq!(mask_modifier_forms("${PLAIN}", TEST_SENTINEL), "${PLAIN}");
    }

    #[test]
    fn mask_helper_masks_default_form() {
        let masked = mask_modifier_forms("${VAR:-x}", TEST_SENTINEL);
        assert_eq!(masked, format!("{TEST_SENTINEL}VAR:-x}}"));
    }

    #[test]
    fn mask_helper_handles_unterminated_brace() {
        assert_eq!(
            mask_modifier_forms("${VAR:-no-close", TEST_SENTINEL),
            "${VAR:-no-close"
        );
    }

    #[test]
    fn mask_helper_passes_bare_form_through() {
        assert_eq!(mask_modifier_forms("$BARE_VAR", TEST_SENTINEL), "$BARE_VAR");
    }

    #[test]
    fn mask_helper_handles_multibyte_chars() {
        let input = "h\u{e9}llo${PLAIN}w\u{f6}rld${VAR:-x}";
        let masked = mask_modifier_forms(input, TEST_SENTINEL);
        let expected = format!("h\u{e9}llo${{PLAIN}}w\u{f6}rld{TEST_SENTINEL}VAR:-x}}");
        assert_eq!(masked, expected);
    }

    // ── Nested / interleaved edge cases ─────────────────────────

    /// Two consecutive modifier forms with no intervening text must both be masked independently.
    #[test]
    fn mask_helper_consecutive_modifier_forms() {
        let masked = mask_modifier_forms("${A:-x}${B:-y}", TEST_SENTINEL);
        assert_eq!(
            masked,
            format!("{TEST_SENTINEL}A:-x}}{TEST_SENTINEL}B:-y}}")
        );
    }

    /// Nested braces inside a modifier body: the walker matches the FIRST closing `}`, so the inner `${B}` becomes part of the outer modifier body.
    /// The literal `${B}` survives inside the masked body for the runtime `sh -c` branch, which handles nesting natively.
    /// The trailing extra `}` has no matching `${` and is left as-is; complex nested expansions are deferred to runtime.
    #[test]
    fn mask_helper_nested_braces_in_modifier_body() {
        let masked = mask_modifier_forms("${A:-${B}}", TEST_SENTINEL);
        assert_eq!(masked, format!("{TEST_SENTINEL}A:-${{B}}}}"));
    }

    /// The closed plain form passes through; the unterminated modifier tail is emitted verbatim because the walker requires a closing `}`.
    #[test]
    fn mask_helper_closed_then_unterminated() {
        let masked = mask_modifier_forms("${A}${B:-", TEST_SENTINEL);
        assert_eq!(masked, "${A}${B:-");
    }

    // ── Sentinel collision regression ──────────────────────────

    /// The previous sentinel was `\x00\x00`; reverting would silently rewrite an input or `extra_env` value containing that byte sequence to `${`.
    #[test]
    fn mask_helper_preserves_pre_existing_old_nul_sentinel() {
        let input = "prefix\u{0}\u{0}suffix";
        assert_eq!(mask_modifier_forms(input, TEST_SENTINEL), input);
    }

    /// Companion to the above: an `extra_env` value containing the OLD sentinel must not be rewritten to `${...}` after expansion.
    #[test]
    fn expand_preserves_pre_existing_old_nul_sentinel_in_extra() {
        let mut extra = HashMap::new();
        // Value contains the legacy 2-NUL sentinel followed by what would parse as an identifier and closing brace
        extra.insert("VAL".to_string(), "\u{0}\u{0}OLD}".to_string());
        let out = expand_env_vars_with_extra("prefix${VAL}suffix", &extra);
        assert_eq!(out, "prefix\u{0}\u{0}OLD}suffix");
        assert!(
            !out.contains("${OLD}"),
            "legacy sentinel must NOT trigger an unmask-to-`${{`, got {out:?}"
        );
    }

    /// An earlier sentinel was the fixed string `"\u{f8ff}__GROK_HOOKS_MASK__\u{f8ff}"`.
    /// A user-supplied `extra_env` value containing that exact byte sequence would have been silently rewritten to `${` by the unmask step.
    /// The per-call randomized sentinel removes this hazard.
    #[test]
    fn expand_preserves_pre_existing_legacy_fixed_sentinel_in_extra() {
        let legacy_sentinel = "\u{f8ff}__GROK_HOOKS_MASK__\u{f8ff}";
        let mut extra = HashMap::new();
        // Value embeds the legacy sentinel followed by what would parse as an identifier and closing brace if the unmask replace had collided
        extra.insert(
            "VAL".to_string(),
            format!("payload-{legacy_sentinel}OLD}}-tail"),
        );
        // Reference VAL via a plain form so its value gets spliced into the output
        let out = expand_env_vars_with_extra("prefix${VAL}suffix", &extra);
        assert_eq!(
            out,
            format!("prefixpayload-{legacy_sentinel}OLD}}-tailsuffix")
        );
        assert!(
            !out.contains("${OLD}"),
            "legacy fixed sentinel must NOT trigger an unmask-to-`${{`, got {out:?}"
        );
    }

    /// Companion: arbitrary high-entropy bytes in an extra-env value must also pass through verbatim.
    /// (Sanity check that the per-call sentinel doesn't collide with random binary content.)
    #[test]
    fn expand_preserves_arbitrary_bytes_in_extra() {
        let mut extra = HashMap::new();
        // Printable ASCII, NULs, PUA chars, brace bytes, and dollar signs: the bytes most likely to clash with a future sentinel scheme
        let exotic = "\u{0}\u{f8ff}${weird}}\u{f8ff}\u{0}__MASK__";
        extra.insert("VAL".to_string(), exotic.to_string());
        let out = expand_env_vars_with_extra("X=${VAL}", &extra);
        assert_eq!(out, format!("X={exotic}"));
    }

    // ── iter_env_var_references unit tests ───────────────────────

    /// Lock down the iterator output for a single braced plain form.
    #[test]
    fn iter_yields_plain_braced_form() {
        let refs: Vec<_> = iter_env_var_references("foo ${BAR} baz").collect();
        let [r] = refs.as_slice() else {
            panic!("expected one ref: {refs:?}");
        };
        assert_eq!(r.name, "BAR");
        assert!(r.braced);
        assert!(!r.has_modifier);
        assert_eq!(r.start, 4);
        assert_eq!(r.end, 10);
    }

    /// Lock down the iterator output for a single bare form.
    #[test]
    fn iter_yields_bare_form() {
        let refs: Vec<_> = iter_env_var_references("foo $BAR baz").collect();
        let [r] = refs.as_slice() else {
            panic!("expected one ref: {refs:?}");
        };
        assert_eq!(r.name, "BAR");
        assert!(!r.braced);
        assert!(!r.has_modifier);
        assert_eq!(r.start, 4);
        assert_eq!(r.end, 8);
    }

    #[test]
    fn iter_flags_modifier_form() {
        let refs: Vec<_> = iter_env_var_references("${VAR:-x}").collect();
        let [r] = refs.as_slice() else {
            panic!("expected one ref: {refs:?}");
        };
        assert_eq!(r.name, "VAR");
        assert!(r.braced);
        assert!(r.has_modifier);
        assert_eq!(r.start, 0);
        assert_eq!(r.end, 9);
    }

    /// Shell positionals / specials / command substitutions are NOT yielded.
    #[test]
    fn iter_skips_shell_specials() {
        let refs: Vec<_> = iter_env_var_references("$1 $$ $? $# $(date) $@").collect();
        assert!(
            refs.is_empty(),
            "shell special params must not yield refs, got {refs:?}"
        );
    }

    /// Unterminated braced form: the `$` is consumed; nothing yielded.
    #[test]
    fn iter_skips_unterminated_brace() {
        let refs: Vec<_> = iter_env_var_references("${VAR:-no-close").collect();
        assert!(refs.is_empty(), "unterminated brace must yield no refs");
    }

    /// Empty / invalid identifier inside braces: yielded with empty name and has_modifier=false.
    #[test]
    fn iter_yields_invalid_braced_form_with_empty_name() {
        let refs: Vec<_> = iter_env_var_references("${:-foo}").collect();
        let [r] = refs.as_slice() else {
            panic!("expected one ref: {refs:?}");
        };
        assert_eq!(r.name, "");
        assert!(r.braced);
        assert!(
            !r.has_modifier,
            "invalid form (no identifier) must not be flagged as a modifier form"
        );
    }

    /// Mixed input: plain, modifier, bare, and a positional.
    #[test]
    fn iter_yields_mixed_forms_in_order() {
        let refs: Vec<_> = iter_env_var_references("${A}${B:-x}$C $1").collect();
        let [a, b, c] = refs.as_slice() else {
            panic!("expected three refs: {refs:?}");
        };
        assert_eq!(a.name, "A");
        assert!(a.braced && !a.has_modifier);
        assert_eq!(b.name, "B");
        assert!(b.braced && b.has_modifier);
        assert_eq!(c.name, "C");
        assert!(!c.braced && !c.has_modifier);
    }

    /// Nested braces are matched at the FIRST `}`; see `mask_helper_nested_braces_in_modifier_body`.
    #[test]
    fn iter_matches_first_closing_brace_for_nested() {
        // The walker reads `A`, sees `:` as the first non-identifier byte, then stops at the FIRST `}`, the inner one at index 8, so end is 9
        // The trailing `}` at index 9 is literal text
        let refs: Vec<_> = iter_env_var_references("${A:-${B}}").collect();
        let [r] = refs.as_slice() else {
            panic!("expected one ref: {refs:?}");
        };
        assert_eq!(r.name, "A");
        assert!(r.braced);
        assert!(r.has_modifier);
        assert_eq!(r.start, 0);
        assert_eq!(r.end, 9);
    }
}
