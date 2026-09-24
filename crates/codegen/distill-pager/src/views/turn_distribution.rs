// Modified for Distill by Samuel Fajreldines, 2026.
//! "Where did this turn go": the per-model/effort distribution block.
//!
//! The shell reports, with the turn's terminal, which model ran each call, at
//! which effort, and how much it cost in tokens (the reasoning model's consults
//! included, on their own rows) — plus why the reasoning model was consulted and
//! how many decisions the Jev layer took. This module turns that payload into
//! the short block that is appended to the scrollback when the turn ends.

use distill_shell::extensions::notification::PromptUsage;

/// The turn's distribution as plain lines, or `None` when there is nothing to
/// report (no turns noted, e.g. replayed turns from an older shell).
pub(crate) fn report(usage: Option<&PromptUsage>) -> Option<String> {
    let usage = usage?;
    let mut lines: Vec<String> = usage
        .effort_usage
        .iter()
        .filter(|row| row.requests > 0 || row.input_tokens + row.output_tokens > 0)
        .map(|row| match &row.effort {
            Some(effort) => format!(
                "{} {} - {} tokens",
                row.model,
                effort,
                format_tokens(row.input_tokens.saturating_add(row.output_tokens))
            ),
            None => format!(
                "{} - {} tokens",
                row.model,
                format_tokens(row.input_tokens.saturating_add(row.output_tokens))
            ),
        })
        .collect();
    if !usage.reasoning_consults.is_empty() {
        lines.push(format!(
            "Reasoning - {}x ({})",
            usage.reasoning_consults.len(),
            usage.reasoning_consults.join(", ")
        ));
    }
    if usage.jev_calls > 0 {
        lines.push(format!("Jev - {}x", usage.jev_calls));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// Compact token count: `950`, `12.4k`, `1.98M`.
pub(crate) fn format_tokens(tokens: u64) -> String {
    match tokens {
        0..=999 => tokens.to_string(),
        1_000..=999_999 => format!("{:.1}k", tokens as f64 / 1_000.0),
        _ => format!("{:.2}M", tokens as f64 / 1_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use distill_shell::extensions::notification::{EffortUsageRow, PromptUsage};

    fn usage(rows: Vec<EffortUsageRow>, jev_calls: u64) -> PromptUsage {
        PromptUsage {
            effort_usage: rows,
            jev_calls,
            ..Default::default()
        }
    }

    fn row(model: &str, effort: Option<&str>, input: u64, output: u64) -> EffortUsageRow {
        EffortUsageRow {
            model: model.to_owned(),
            effort: effort.map(str::to_owned),
            requests: 1,
            input_tokens: input,
            output_tokens: output,
        }
    }

    /// The block reads like the owner asked: `<model> <effort> - <tokens> tokens`
    /// per engine that ran, then the Jev call count.
    #[test]
    fn the_report_names_each_engine_and_the_jev_calls() {
        let usage = usage(
            vec![
                row("Qwen3.8 27B (local oMLX)", None, 1_800_000, 180_000),
                row("DeepSeek V4.1 Flash", Some("high"), 900_000, 100_000),
                row("DeepSeek V4.1 Flash", Some("medium"), 480_000, 20_000),
            ],
            41,
        );
        let report = report(Some(&usage)).expect("a report");
        assert_eq!(
            report,
            "Qwen3.8 27B (local oMLX) - 1.98M tokens\n\
             DeepSeek V4.1 Flash high - 1.00M tokens\n\
             DeepSeek V4.1 Flash medium - 500.0k tokens\n\
             Jev - 41x"
        );
    }

    /// The reasoning model's tokens read like the main model's (its own row, with
    /// its effort), and the block says why it was consulted.
    #[test]
    fn the_report_shows_the_reasoning_model_like_the_main_one() {
        let mut usage = usage(
            vec![
                row("GPT-6-Luna (ChatGPT)", Some("medium"), 5_000_000, 140_000),
                row("GPT-6-Sol (ChatGPT)", Some("high"), 38_000, 4_100),
            ],
            12,
        );
        usage.reasoning_consults = vec!["plan".to_owned(), "review".to_owned()];
        assert_eq!(
            report(Some(&usage)).expect("a report"),
            "GPT-6-Luna (ChatGPT) medium - 5.14M tokens\n\
             GPT-6-Sol (ChatGPT) high - 42.1k tokens\n\
             Reasoning - 2x (plan, review)\n\
             Jev - 12x"
        );
    }

    /// Nothing to say ⇒ no block: a turn the decision layer never touched and
    /// whose usage was not noted must not add noise to the scrollback.
    #[test]
    fn an_empty_payload_reports_nothing() {
        assert!(report(None).is_none());
        assert!(report(Some(&PromptUsage::default())).is_none());
        // Jev calls alone still say something.
        let only_jev = usage(Vec::new(), 7);
        assert_eq!(report(Some(&only_jev)).as_deref(), Some("Jev - 7x"));
    }

    #[test]
    fn token_formatting_stays_short() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(1_000), "1.0k");
        assert_eq!(format_tokens(12_400), "12.4k");
        assert_eq!(format_tokens(999_999), "1000.0k");
        assert_eq!(format_tokens(1_980_000), "1.98M");
    }
}
