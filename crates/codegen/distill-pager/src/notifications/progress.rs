use crate::notifications::tmux;
use crate::terminal::{TerminalContext, TerminalName};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressState {
    Indeterminate,
    Clear,
}

pub fn supports_progress_bar(ctx: &TerminalContext) -> bool {
    match ctx.brand {
        TerminalName::Ghostty | TerminalName::WezTerm => true,
        // iTerm2 added OSC 9;4 progress support in 3.6
        // Older versions misinterpret the sequence as an OSC 9 desktop notification, displaying the raw parameters (e.g. "4;1;-1") as alert text.
        TerminalName::Iterm2 => ctx.is_term_program_version_or_later(3, 6),
        _ => false,
    }
}

const OSC_INDETERMINATE: &str = "\x1b]9;4;1;-1\x07";
pub(crate) const OSC_CLEAR: &str = "\x1b]9;4;0;0\x07";

/// Build the progress bar escape sequence without writing it.
///
/// Returns `None` if the terminal brand does not support the OSC 9;4 progress indicator.
pub fn build_progress_escape(state: ProgressState, ctx: &TerminalContext) -> Option<String> {
    if !supports_progress_bar(ctx) {
        return None;
    }
    let sequence = match state {
        ProgressState::Indeterminate => OSC_INDETERMINATE,
        ProgressState::Clear => OSC_CLEAR,
    };
    if tmux::passthrough_available(ctx) {
        Some(tmux::tmux_passthrough(sequence))
    } else {
        Some(sequence.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{MultiplexerKind, TerminalContext};

    fn ctx_for(brand: TerminalName) -> TerminalContext {
        TerminalContext {
            brand,
            ..Default::default()
        }
    }

    #[test]
    fn supported_brands() {
        assert!(supports_progress_bar(&ctx_for(TerminalName::Ghostty)));
        assert!(supports_progress_bar(&ctx_for(TerminalName::WezTerm)));
    }

    #[test]
    fn unsupported_brands() {
        let unsupported = [
            TerminalName::Kitty,
            TerminalName::Alacritty,
            TerminalName::AppleTerminal,
            TerminalName::VsCode,
            TerminalName::WarpTerminal,
            TerminalName::GrokDesktop,
            TerminalName::Vte,
            TerminalName::Unknown,
        ];
        for brand in unsupported {
            assert!(
                !supports_progress_bar(&ctx_for(brand)),
                "{brand:?} should not support progress bar"
            );
        }
    }

    #[test]
    fn escape_none_for_unsupported_terminal() {
        let ctx = ctx_for(TerminalName::Kitty);
        assert_eq!(
            build_progress_escape(ProgressState::Indeterminate, &ctx),
            None
        );
        assert_eq!(build_progress_escape(ProgressState::Clear, &ctx), None);
    }

    #[test]
    fn escape_built_for_supported_terminals() {
        for ctx in [
            TerminalContext {
                brand: TerminalName::Iterm2,
                term_program_version: Some("3.6.0".into()),
                ..Default::default()
            },
            ctx_for(TerminalName::Ghostty),
            ctx_for(TerminalName::WezTerm),
        ] {
            assert_eq!(
                build_progress_escape(ProgressState::Indeterminate, &ctx).as_deref(),
                Some(OSC_INDETERMINATE)
            );
            assert_eq!(
                build_progress_escape(ProgressState::Clear, &ctx).as_deref(),
                Some(OSC_CLEAR)
            );
        }
    }

    #[test]
    fn escape_wrapped_for_tmux_passthrough() {
        let ctx = TerminalContext {
            brand: TerminalName::Iterm2,
            multiplexer: MultiplexerKind::Tmux,
            tmux_version: Some("tmux 3.3".into()),
            term_program_version: Some("3.6.0".into()),
            ..Default::default()
        };
        let esc = build_progress_escape(ProgressState::Indeterminate, &ctx).expect("escape");
        assert!(esc.starts_with("\x1bPtmux;"), "missing DCS passthrough");
        assert!(
            esc.ends_with("\x1b\\"),
            "expected ST terminator, got: {esc:?}",
        );
    }
}
