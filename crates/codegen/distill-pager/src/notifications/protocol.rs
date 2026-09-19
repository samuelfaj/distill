use std::borrow::Cow;

use crate::notifications::tmux;
use crate::terminal::{MultiplexerKind, TerminalContext, TerminalName};

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub enum NotificationProtocol {
    /// iTerm2/WezTerm/Warp: `\x1b]9;{message}\x07`
    Osc9,
    /// Kitty: `\x1b]99;i=grok;{message}\x1b\\`
    Osc99,
    /// Ghostty/VTE: `\x1b]777;notify;{title};{body}\x1b\\`
    Osc777,
    /// Universal fallback: `\x07`
    Bel,
    /// No notification capability
    None,
}
/// Choose the best notification protocol for the current terminal environment.
pub fn select_protocol(ctx: &TerminalContext) -> NotificationProtocol {
    if ctx.multiplexer == MultiplexerKind::Zellij {
        return NotificationProtocol::Bel;
    }
    match ctx.brand {
        TerminalName::Iterm2 | TerminalName::WezTerm | TerminalName::WarpTerminal => {
            NotificationProtocol::Osc9
        }
        TerminalName::Kitty => NotificationProtocol::Osc99,
        TerminalName::Ghostty
        | TerminalName::Vte
        | TerminalName::Terminator
        | TerminalName::Foot => NotificationProtocol::Osc777,
        TerminalName::GrokDesktop => NotificationProtocol::None,
        TerminalName::AppleTerminal
        | TerminalName::Alacritty
        | TerminalName::Rio
        | TerminalName::VsCode
        | TerminalName::WindowsTerminal
        | TerminalName::JetBrains
        | TerminalName::Cursor
        | TerminalName::Windsurf
        | TerminalName::Zed
        | TerminalName::Otty
        | TerminalName::Unknown => NotificationProtocol::Bel,
    }
}

const BEL_BYTE: &[u8] = b"\x07";

/// Strip C0/C1 controls (including BEL and C1 ST) so model-derived titles cannot terminate OSC/DCS early.
/// Same filter as tab-title construction.
fn sanitize_osc_text(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Build the OSC/BEL payload. `None` means emit nothing.
fn notification_sequence(
    protocol: NotificationProtocol,
    title: &str,
    body: &str,
) -> Option<Cow<'static, str>> {
    let title = sanitize_osc_text(title);
    let body = sanitize_osc_text(body);
    Some(match protocol {
        // Body-only protocols fold the title (session name) into the body.
        // OSC 777 already uses the tab title as subtitle, so keep "Grok".
        NotificationProtocol::Osc9 => format!("\x1b]9;{body} \u{b7} {title}\x07").into(),
        NotificationProtocol::Osc99 => format!("\x1b]99;i=grok;{body} \u{b7} {title}\x1b\\").into(),
        NotificationProtocol::Osc777 => format!("\x1b]777;notify;Grok;{body}\x1b\\").into(),
        NotificationProtocol::Bel => Cow::Borrowed("\x07"),
        NotificationProtocol::None => return None,
    })
}

/// Build the notification bytes ready for the tty. `None` means emit nothing.
///
/// When running under tmux the sequence is wrapped in DCS passthrough so the outer terminal sees it.
fn notification_bytes(
    protocol: NotificationProtocol,
    title: &str,
    body: &str,
    ctx: &TerminalContext,
) -> Option<Vec<u8>> {
    let sequence = notification_sequence(protocol, title, body)?;
    Some(if ctx.is_tmux_backed() {
        tmux::tmux_passthrough(&sequence).into_bytes()
    } else if matches!(protocol, NotificationProtocol::Bel) {
        BEL_BYTE.to_vec()
    } else {
        sequence.into_owned().into_bytes()
    })
}

/// Build the escape sequence for a notification and enqueue it on the terminal writer
/// (notifications fire from the event-loop thread; see `EscapeWriter`).
pub fn emit_notification(
    protocol: NotificationProtocol,
    title: &str,
    body: &str,
    ctx: &TerminalContext,
    writer: &crate::render::draw::EscapeWriter,
) {
    if let Some(bytes) = notification_bytes(protocol, title, body, ctx) {
        writer.emit(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{MultiplexerKind, TerminalContext, TerminalName};

    fn ctx_with_brand(brand: TerminalName) -> TerminalContext {
        TerminalContext {
            brand,
            ..Default::default()
        }
    }

    fn ctx_with_brand_and_mux(brand: TerminalName, mux: MultiplexerKind) -> TerminalContext {
        TerminalContext {
            brand,
            multiplexer: mux,
            ..Default::default()
        }
    }

    // tmux does NOT override protocol selection (passthrough is handled at emission time, not selection time)

    #[test]
    fn tmux_preserves_osc9_for_iterm2() {
        assert_eq!(
            select_protocol(&ctx_with_brand_and_mux(
                TerminalName::Iterm2,
                MultiplexerKind::Tmux
            )),
            NotificationProtocol::Osc9
        );
    }

    #[test]
    fn tmux_preserves_osc99_for_kitty() {
        assert_eq!(
            select_protocol(&ctx_with_brand_and_mux(
                TerminalName::Kitty,
                MultiplexerKind::Tmux
            )),
            NotificationProtocol::Osc99
        );
    }

    // screen does not override

    #[test]
    fn screen_preserves_osc777_for_ghostty() {
        assert_eq!(
            select_protocol(&ctx_with_brand_and_mux(
                TerminalName::Ghostty,
                MultiplexerKind::Screen
            )),
            NotificationProtocol::Osc777
        );
    }

    #[test]
    fn bytes_none_is_noop() {
        let ctx = ctx_with_brand(TerminalName::GrokDesktop);
        assert_eq!(
            notification_bytes(NotificationProtocol::None, "title", "body", &ctx),
            None
        );
    }

    #[test]
    fn bytes_bel_is_bare_bell() {
        let ctx = ctx_with_brand(TerminalName::Unknown);
        assert_eq!(
            notification_bytes(NotificationProtocol::Bel, "", "", &ctx).as_deref(),
            Some(b"\x07".as_slice())
        );
    }

    #[test]
    fn bytes_osc9_folds_title_into_body() {
        let ctx = ctx_with_brand(TerminalName::Iterm2);
        assert_eq!(
            notification_bytes(NotificationProtocol::Osc9, "title", "body", &ctx).as_deref(),
            Some("\x1b]9;body \u{b7} title\x07".as_bytes())
        );
    }

    #[test]
    fn bytes_tmux_backed_wraps_in_dcs_passthrough() {
        let ctx = ctx_with_brand_and_mux(TerminalName::Iterm2, MultiplexerKind::Tmux);
        let bytes = notification_bytes(NotificationProtocol::Osc9, "t", "b", &ctx).expect("bytes");
        assert!(bytes.starts_with(b"\x1bPtmux;"), "missing DCS passthrough");
    }

    fn capture_writer() -> (
        crate::render::draw::EscapeWriter,
        std::sync::mpsc::Receiver<crate::render::draw::WriterPayload>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let writer =
            crate::render::draw::EscapeWriter::new(tx, crate::render::draw::WriterSync::new());
        (writer, rx)
    }

    /// The emit path enqueues the built bytes on the writer queue (never an inline tty write).
    #[test]
    fn emit_enqueues_bytes_on_writer_queue() {
        let ctx = ctx_with_brand(TerminalName::Ghostty);
        let (writer, rx) = capture_writer();
        emit_notification(NotificationProtocol::Osc777, "title", "body", &ctx, &writer);
        let payload = rx.try_recv().expect("one payload enqueued");
        assert_eq!(payload.data(), "\x1b]777;notify;Grok;body\x1b\\".as_bytes());
    }

    #[test]
    fn emit_none_enqueues_nothing() {
        let ctx = ctx_with_brand(TerminalName::GrokDesktop);
        let (writer, rx) = capture_writer();
        emit_notification(NotificationProtocol::None, "title", "body", &ctx, &writer);
        assert!(rx.try_recv().is_err(), "no payload expected");
    }

    #[test]
    fn notification_sequence_strips_controls_from_title_and_body() {
        let seq = notification_sequence(
            NotificationProtocol::Osc9,
            "ti\x1btle\u{9c}",
            "bo\x07dy\x18",
        )
        .expect("osc9 yields a sequence");
        assert_eq!(seq.as_ref(), "\x1b]9;body \u{b7} title\x07");
    }

    #[test]
    fn notification_sequence_osc99_and_osc777_strip_controls() {
        let osc99 = notification_sequence(NotificationProtocol::Osc99, "t\x1b", "b\x07")
            .expect("osc99 yields a sequence");
        assert_eq!(osc99.as_ref(), "\x1b]99;i=grok;b \u{b7} t\x1b\\");

        let osc777 = notification_sequence(NotificationProtocol::Osc777, "ignored\x1b", "b\x1body")
            .expect("osc777 yields a sequence");
        assert_eq!(osc777.as_ref(), "\x1b]777;notify;Grok;body\x1b\\");
    }

    #[test]
    fn notification_sequence_none_is_none() {
        assert!(notification_sequence(NotificationProtocol::None, "t", "b").is_none());
    }

    #[test]
    fn all_brands_have_defined_protocol() {
        let cases: &[(TerminalName, NotificationProtocol)] = &[
            (TerminalName::Iterm2, NotificationProtocol::Osc9),
            (TerminalName::WezTerm, NotificationProtocol::Osc9),
            (TerminalName::WarpTerminal, NotificationProtocol::Osc9),
            (TerminalName::Kitty, NotificationProtocol::Osc99),
            (TerminalName::Ghostty, NotificationProtocol::Osc777),
            (TerminalName::Vte, NotificationProtocol::Osc777),
            (TerminalName::Foot, NotificationProtocol::Osc777),
            (TerminalName::GrokDesktop, NotificationProtocol::None),
            (TerminalName::AppleTerminal, NotificationProtocol::Bel),
            (TerminalName::Alacritty, NotificationProtocol::Bel),
            (TerminalName::VsCode, NotificationProtocol::Bel),
            (TerminalName::WindowsTerminal, NotificationProtocol::Bel),
            (TerminalName::Unknown, NotificationProtocol::Bel),
        ];

        for &(brand, expected) in cases {
            let ctx = ctx_with_brand(brand);
            assert_eq!(
                select_protocol(&ctx),
                expected,
                "protocol mismatch for {brand:?}"
            );
        }
    }

    #[test]
    fn zellij_forces_bel_for_all_osc_brands() {
        let osc_brands = [
            TerminalName::Iterm2,
            TerminalName::WezTerm,
            TerminalName::WarpTerminal,
            TerminalName::Kitty,
            TerminalName::Ghostty,
            TerminalName::Vte,
        ];
        for brand in osc_brands {
            let ctx = ctx_with_brand_and_mux(brand, MultiplexerKind::Zellij);
            assert_eq!(
                select_protocol(&ctx),
                NotificationProtocol::Bel,
                "zellij should force BEL for {brand:?}"
            );
        }
    }
}
