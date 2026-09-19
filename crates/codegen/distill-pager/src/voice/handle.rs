// Modified for Distill by Samuel Fajreldines, 2026.
//! Applies [`VoiceEvent`]s from the voice pipeline to the prompt text and the dictation overlay.

use std::ops::Range;

use distill_voice::VoiceEvent;

use crate::app::app_view::{AppView, VoiceTarget};
use crate::views::prompt_widget::PromptWidget;

/// Whether a draft counts as blank for voice insertion: an empty or whitespace-only draft is
/// replaced wholesale rather than dictated into. Shared by the insert, submit-merge, and ghost
/// preview paths so they agree on what "blank" means.
pub(crate) fn prompt_blank_for_voice(text: &str) -> bool {
    text.trim().is_empty()
}

/// Wraps a voice `fragment` with a leading and/or trailing space so it reads as its own word at
/// byte offset `at` in `text`, adding each space only where the neighbor is non-whitespace.
///
/// `text`/`at` must describe the buffer as it will look when the fragment lands — with any active
/// selection already removed — so the neighbors are the characters that actually end up adjacent.
/// `at` must be a UTF-8 char boundary. The insertion, the submit merge, and the ghost preview all
/// route through this so their spacing cannot drift.
pub(crate) fn space_voice_fragment(text: &str, at: usize, fragment: &str) -> String {
    let needs_leading = at > 0
        && text
            .get(..at)
            .is_some_and(|prefix| !prefix.ends_with(char::is_whitespace));
    let needs_trailing = at < text.len()
        && text
            .get(at..)
            .is_some_and(|suffix| !suffix.starts_with(char::is_whitespace));
    let mut out = String::with_capacity(fragment.len() + 2);
    if needs_leading {
        out.push(' ');
    }
    out.push_str(fragment);
    if needs_trailing {
        out.push(' ');
    }
    out
}

/// Merges a voice `fragment` into `existing`, replacing `replace` — the selection captured at
/// commit time, or an empty range at the caret. A blank draft is replaced outright; `None` (or a
/// range at/after the end) appends. Spacing is computed against the post-delete buffer so the merge
/// matches the live insertion, which replaces the selection rather than keeping it.
pub(crate) fn merge_voice_fragment(
    existing: &str,
    replace: Option<Range<usize>>,
    fragment: &str,
) -> String {
    if prompt_blank_for_voice(existing) {
        return fragment.to_string();
    }
    let floor = |i: usize| {
        let mut i = i.min(existing.len());
        while i > 0 && !existing.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let (start, end) = match replace {
        Some(range) => {
            let start = floor(range.start);
            (start, floor(range.end).max(start))
        }
        None => (existing.len(), existing.len()),
    };
    // Spacing is decided against the buffer as it looks once the selected span is gone.
    let Some(prefix) = existing.get(..start) else {
        return fragment.to_string();
    };
    let Some(suffix) = existing.get(end..) else {
        return fragment.to_string();
    };
    let base = format!("{prefix}{suffix}");
    let insertion = space_voice_fragment(&base, start, fragment);
    let mut merged = String::with_capacity(base.len() + insertion.len());
    merged.push_str(prefix);
    merged.push_str(&insertion);
    merged.push_str(suffix);
    merged
}

/// A promoted interim fragment plus the span it replaced, so a submit path that merges a separately
/// captured payload can drop the fragment at the same place — replacing the same selection — instead
/// of appending it.
pub(crate) struct VoiceInterimCommit {
    pub fragment: String,
    /// Byte range replaced in the bound draft: the active selection at commit time, or an empty
    /// range at the caret. `None` only when the draft was unreachable (append fallback).
    pub replace: Option<Range<usize>>,
}

/// The prompt bound to the active voice session (agent or dashboard), if it is reachable.
/// The shared peek-reply box only resolves while its row is still the peeked one.
fn bound_voice_prompt_mut(app: &mut AppView) -> Option<&mut PromptWidget> {
    match app.voice_recording_target()? {
        VoiceTarget::Agent(id) => app.agents.get_mut(&id).map(|agent| &mut agent.prompt),
        target @ (VoiceTarget::DashboardDispatch | VoiceTarget::DashboardPeekReply(_)) => {
            let dashboard = app.dashboard.as_mut()?;
            match target {
                VoiceTarget::DashboardPeekReply(rec) => {
                    let peeked = match dashboard.peek.as_ref().map(|p| &p.row) {
                        Some(crate::views::dashboard::DashboardRowId::TopLevel(id)) => Some(*id),
                        _ => None,
                    };
                    if peeked != Some(rec) {
                        return None;
                    }
                    Some(&mut dashboard.peek_reply)
                }
                _ => Some(&mut dashboard.dispatch),
            }
        }
    }
}

/// Inserts `fragment` at the caret with smart spacing; replaces a blank draft outright.
fn insert_voice_fragment_into_widget(prompt: &mut PromptWidget, fragment: &str) {
    let existing = prompt.text();
    if prompt_blank_for_voice(existing) {
        prompt.set_text(fragment);
        prompt.set_cursor(fragment.len());
        return;
    }
    // insert_replacing_selection deletes any active selection before inserting, so the neighbors
    // that decide spacing are the characters left AFTER the selection is gone — not the highlighted
    // ones. Compute the spacing against that post-delete buffer at the selection's start.
    let (base, at) = match prompt.selection_range() {
        Some(sel) => match (existing.get(..sel.start), existing.get(sel.end..)) {
            (Some(prefix), Some(suffix)) => (format!("{prefix}{suffix}"), sel.start),
            _ => (existing.to_owned(), prompt.cursor()),
        },
        None => (existing.to_owned(), prompt.cursor()),
    };
    let insertion = space_voice_fragment(&base, at, fragment);
    // Route through insert_replacing_selection so dictation over a selection replaces it the way
    // typed input would, and any image chips the selection spanned get resynced.
    prompt.insert_replacing_selection(&insertion);
}

/// Inserts `text` at the caret of the prompt bound at capture start (agent or dashboard).
fn insert_voice_text_into_prompt(app: &mut AppView, text: &str) {
    if let Some(prompt) = bound_voice_prompt_mut(app) {
        insert_voice_fragment_into_widget(prompt, text);
    }
}

/// Move non-empty interim into the bound prompt and clear the overlay.
/// Does not stop the mic. Returns the promoted fragment and the caret it landed at.
pub(crate) fn commit_interim_into_prompt(app: &mut AppView) -> Option<VoiceInterimCommit> {
    let interim = app
        .voice_interim()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)?;
    // Span replaced in the bound draft before insertion — the active selection, or an empty range
    // at the caret — so a submit path merging a separately captured payload replaces the same span
    // the live insertion does instead of leaving the selected text in place.
    let replace = bound_voice_prompt_mut(app).map(|prompt| match prompt.selection_range() {
        Some(sel) => sel,
        None => {
            let caret = prompt.cursor();
            caret..caret
        }
    });
    insert_voice_text_into_prompt(app, &interim);
    app.voice_clear_interim();
    Some(VoiceInterimCommit {
        fragment: interim,
        replace,
    })
}

/// Apply a voice event to app state. Returns whether the frame should redraw.
pub fn handle_voice_event(app: &mut AppView, event: VoiceEvent) -> bool {
    match event {
        VoiceEvent::InterimTranscript { text } => {
            // No-op unless recording, so a late interim after a stop can't repopulate the overlay
            app.voice_set_interim(text)
        }
        VoiceEvent::UtteranceFinal { text } => {
            app.voice_clear_interim();
            // The mic stays open across pauses; the user stops it explicitly, then presses Enter to send
            // The bound target survives a stop (`Stopping`), so a trailing final after an explicit stop still lands
            if !text.trim().is_empty() {
                insert_voice_text_into_prompt(app, text.trim());
            }
            true
        }
        VoiceEvent::Error { message, hint } => {
            let target = app.voice_recording_target();
            app.voice_reset();
            app.show_toast(&format!("Voice: {message}"));
            // The hint holds long fix steps, so it goes to the agent or peek scrollback; a toast is one line, and dashboard dispatch has no scrollback
            if let Some(hint) = hint
                && let Some(VoiceTarget::Agent(id) | VoiceTarget::DashboardPeekReply(id)) = target
                && let Some(agent) = app.agents.get_mut(&id)
            {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(format!(
                        "Voice: {message}. {hint}"
                    )));
            }
            true
        }
    }
}
