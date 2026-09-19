//! Welcome screen: the first thing users see.
//!
//! Layout (top to bottom):
//! - Top margin row (always preserved)
//! - Top bar: `{branch} worktree {cwd}` from [`location_parts`](crate::views::location::location_parts)
//! - Vertically centered content: logo, gap, menu, gap, prompt
//! - Bottom margin

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Widget, Wrap};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::app_view::{AuthMode, AuthState, SessionPickerEntry, TrustState};
use crate::app::consent::ConsentState;
use crate::startup::StartupWarning;
use crate::theme::Theme;
use crate::views::prompt_widget::{PromptFlag, PromptInfo, PromptWidget};
use crate::views::session_picker_surface::{
    SessionPickerRenderCtx, SessionPickerRenderMode, render_session_picker,
};
mod cat;
mod consent;
mod hero_box;
pub(crate) mod logo;
mod menu;
mod prompt;
mod toast;
mod top_bar;
#[cfg(feature = "local-workspace")]
pub(crate) mod workspace_mode;

/// Compact welcome/session/waiting/dock share this one-col left gutter.
pub(crate) const PROMPT_GUTTER: u16 = 1;

pub(crate) use logo::shimmer_frame;
use logo::{LogoTier, logo_line_count, render_logo, render_logo_tier, title_spans};

/// Product name shown in the welcome banner (the art above it shimmers
/// with the same sweep, so the two animate as one piece).
const WORDMARK: &str = "Remote-Code  ";
use menu::render_menu;
pub(crate) use toast::paint_welcome_toast;
use top_bar::render_top_bar;
#[cfg(feature = "local-workspace")]
pub use workspace_mode::{
    WelcomeWorkspaceMode, WorkspaceModeHitRects, hit_test_workspace_mode,
    render_workspace_mode_picker,
};

/// True for VS Code and xterm.js embeds (VS Code-family IDEs and Zed) where quit is `Ctrl+D` (canonical: [`TerminalName::is_vscode_family`]).
fn welcome_in_vscode_family() -> bool {
    crate::terminal::terminal_context().brand.is_vscode_family()
}

/// Build the quit hint spans used in Authenticating sub-screens.
fn quit_hint_spans(theme: &Theme) -> Vec<Span<'static>> {
    let key = if welcome_in_vscode_family() {
        "ctrl+d"
    } else {
        "ctrl+q"
    };
    vec![
        Span::styled(
            key,
            Style::default()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  quit", Style::default().fg(theme.gray)),
    ]
}

/// Style for a clickable welcome block: bright primary while `hovered`, else `base`.
/// Shared by the announcement and changelog renderers.
pub(super) fn hover_style(theme: &Theme, hovered: bool, base: Style) -> Style {
    if hovered {
        // Brighten to the primary fg; on the terminal palette the base fgs
        // and text_primary are all Reset, so bold carries the cue instead.
        if theme.is_bandless() {
            Style::default()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_primary)
        }
    } else {
        base
    }
}

/// Takes the version badge's row on screens with no shortcuts bar.
pub(super) fn render_pending_hint(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    pending: &crate::views::shortcuts_bar::PendingHint,
) {
    let key_style = Style::default()
        .fg(theme.text_primary)
        .add_modifier(Modifier::BOLD);
    let action_style = Style::default().fg(theme.gray);
    let line = Line::from(vec![
        Span::styled(format!("  {}", pending.shortcut.display()), key_style),
        Span::styled(":", action_style),
        Span::styled(format!("press again to {}", pending.label), action_style),
    ]);
    buf.set_line(area.x, area.y, &line, area.width);
}

/// Horizontal margin (left and right) in normal mode.
const H_MARGIN: u16 = 2;
/// Horizontal margin in compact mode.
const H_MARGIN_COMPACT: u16 = 1;

/// Minimum width for the menu and changelog sections so they don't resize when the import row toggles.
/// Derivation: "[ " (2) + import-claude label (22) + gap (4) + "ctrl+i  [x]" (11) + " ]" (2) = 41.
/// The extra 10 columns leave breathing room.
const MENU_MIN_WIDTH: u16 = 51;

/// Whether the welcome prompt is currently focused (accepting text input).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WelcomePromptFocus {
    #[default]
    Unfocused,
    Focused,
}

/// Result of rendering the welcome screen.
#[derive(Default)]
pub struct WelcomeRenderResult {
    /// Cursor position (if the prompt wants a visible cursor).
    pub cursor_pos: Option<(u16, u16)>,
    /// Terminal image/cursor escapes paired with their ownership transition.
    pub post_flush_escapes: Option<crate::terminal::overlay::PostFlush>,
    /// Hit-test rects for each menu item (for click/hover).
    pub menu_rects: Vec<Rect>,
    /// Hit-test rect for the prompt input area (for click to start session).
    pub prompt_rect: Option<Rect>,
    /// Hit-test rect for the import-claude banner (for click to open import modal).
    pub import_banner_rect: Option<Rect>,
    /// Hit areas from the session picker (for mouse hit-testing).
    pub session_picker_hit_areas: Option<crate::views::picker::PickerHitAreas>,
    /// Hit-test rect for the auth copy line (click-to-copy during Authenticating).
    pub auth_url_rect: Option<Rect>,
    /// Hit-test rect for the "show full URL" fallback link.
    pub auth_fallback_rect: Option<Rect>,
    /// Hit-test rect for the "[Refresh]" button on the paywall tier line.
    pub refresh_rect: Option<Rect>,
    /// Hit-test rect for the gate URL link (click to open in browser).
    pub gate_url_rect: Option<Rect>,
    /// Hit-test rects for the inline links, tagged with their index, one per row a link wraps to.
    pub consent_link_rects: Vec<(usize, Rect)>,
    /// `None` when this frame did not paint the notice.
    pub consent_legibility: Option<crate::app::consent::ConsentLegibility>,
    /// Hit-test rect for the clickable changelog info block (opens release notes).
    pub changelog_cta_rect: Option<Rect>,
    /// Whether the announcement overflowed (the "expandable" signal).
    pub announcement_truncated: bool,
    /// Hit-test rect for the full announcement block (click anywhere to toggle).
    pub announcement_rect: Option<Rect>,
    /// Hit-test rect for the promo upgrade CTA `[label]` button (click to open).
    pub upgrade_cta_rect: Option<Rect>,
    pub privacy_banner_opt_in_rect: Option<Rect>,
    pub privacy_banner_opt_out_rect: Option<Rect>,
    pub privacy_banner_terms_rect: Option<Rect>,
    pub privacy_banner_policy_rect: Option<Rect>,
    /// Hit-test rects for the chat workspace-mode segmented control.
    #[cfg(feature = "local-workspace")]
    pub workspace_mode_rects: WorkspaceModeHitRects,
}

use hero_box::HERO_BOX_MIN_WIDTH;

/// Prompt input height for a one-line draft (shared across hero and stacked layout paths).
const PROMPT_HEIGHT: u16 = 3;

/// Half the content area, as in the session view, but never more than the logo-less stacked column leaves beside the menu, tip, version and announcement rows.
/// The draft can therefore push the logo out, never the announcement.
fn prompt_max_height(input: &WelcomeLayoutInput<'_>) -> u16 {
    let content_height = input.content_area.height;
    let column = StackedColumn::new(input, 0);
    let info = stacked_announcement_rows(input);
    let info_gap = if info > 0 { 1u16 } else { 0 };
    let column_fit = content_height.saturating_sub(
        column.fixed_above(LogoTier::Hidden)
            + input.menu_height
            + info_gap
            + info
            + 1
            + column.fixed_below,
    );
    (content_height / 2).min(column_fit).max(PROMPT_HEIGHT)
}

/// Rows the stacked info slot gives the announcement: its desired rows at the stacked slot width, clamped to what fits
/// beside a one-line prompt with the logo hidden. Independent of the draft, so a growing draft never reflows it.
/// Zero without an announcement or in compact mode (the picker owns the column).
fn stacked_announcement_rows(input: &WelcomeLayoutInput<'_>) -> u16 {
    let Some(ann) = input.announcement else {
        return 0;
    };
    if input.compact {
        return 0;
    }
    let content_area = input.content_area;
    let avail = content_area
        .width
        .saturating_sub(prompt::prompt_inset(input.prompt_compact) * 2);
    let width = stacked_info_width(avail, content_area.height, MENU_MIN_WIDTH);
    let column = StackedColumn::new(input, PROMPT_HEIGHT);
    // +1 info-slot gap, +1 min flex gap above the tip
    let budget = content_area.height.saturating_sub(
        column.fixed_above(LogoTier::Hidden) + input.menu_height + 1 + column.fixed_below + 1,
    );
    hero_box::announcement_desired_rows(ann, width, input.expanded, input.has_upgrade_cta)
        .min(budget)
}

/// The stacked column's rows around the logo, for one prompt height.
struct StackedColumn {
    content_height: u16,
    gap_after_logo: u16,
    error_height: u16,
    menu_height: u16,
    fixed_below: u16,
}

impl StackedColumn {
    fn new(input: &WelcomeLayoutInput<'_>, prompt_height: u16) -> Self {
        Self {
            content_height: input.content_area.height,
            gap_after_logo: if input.error_height > 0 { 1 } else { 0 },
            error_height: input.error_height,
            menu_height: input.menu_height,
            fixed_below: WelcomeLayout::fixed_below(input.tip_height, prompt_height),
        }
    }

    /// Logo rows, the gap and wordmark under them, and the error block.
    fn fixed_above(&self, tier: LogoTier) -> u16 {
        tier.rows() + 1 + WORDMARK_ROWS + self.gap_after_logo + self.error_height
    }

    /// Whether the column fits with this tier beside `reserved` info rows (slot + gap) and a one-row flex gap.
    fn fits(&self, tier: LogoTier, reserved: u16) -> bool {
        self.fixed_above(tier) + self.menu_height + reserved + 1 + self.fixed_below
            <= self.content_height
    }

    /// The tier the terminal height allows, stepped down only while the column would overflow beside the draft and the reserved rows.
    fn logo_tier(&self, reserved: u16) -> LogoTier {
        let mut tier = LogoTier::for_height(self.content_height);
        while !self.fits(tier, reserved) {
            match tier.step_down() {
                Some(smaller) => tier = smaller,
                None => break,
            }
        }
        tier
    }
}
/// Gap between prompt and version line.
const VERSION_GAP: u16 = 1;

/// The wordmark row under the cat: the name and the version, one line.
const WORDMARK_ROWS: u16 = 1;

/// Computed areas for the welcome screen vertical layout.
pub(super) struct WelcomeLayout {
    pub(super) logo: Rect,
    /// The wordmark line under the cat: `Remote-Code` and the version.
    pub(super) wordmark: Rect,
    pub(super) error: Rect,
    pub(super) menu: Rect,
    /// Stacked info slot below the menu (narrow layout only): it shows either the announcement or the changelog (the announcement takes priority).
    /// Zero in the hero box layout, which uses `hero_info` instead.
    pub(super) changelog: Rect,
    pub(super) tip: Rect,
    pub(super) prompt: Rect,
    pub(super) version: Rect,
    // Hero box sub-rects (all zero when hero box is inactive).
    pub(super) hero_box: Rect,
    pub(super) hero_logo: Rect,
    pub(super) hero_version: Rect,
    pub(super) hero_subtitle: Rect,
    /// In-box info slot: it shows either the announcement or the changelog (the announcement takes priority).
    pub(super) hero_info: Rect,
    pub(super) hero_menu: Rect,
    /// The art the stacked `logo` rows were reserved for; paint it with [`render_logo_tier`].
    pub(super) logo_tier: LogoTier,
}

/// Inputs to [`WelcomeLayout::compute`] / [`WelcomeLayout::compute_stacked`].
///
/// Bundled (and `Default`-able) so call sites name each field; in particular the two distinct compaction flags can't be silently transposed.
#[derive(Default)]
struct WelcomeLayoutInput<'a> {
    content_area: Rect,
    /// Error/warning row height; 0 when there's nothing to show.
    error_height: u16,
    menu_height: u16,
    tip_height: u16,
    /// Desired changelog height (collapsed to 0 if the terminal is too short).
    changelog_height: u16,
    /// Vertical compaction (session picker visible): skip the logo and the info slot.
    compact: bool,
    /// Horizontal-inset compaction (appearance setting) for the stacked slot.
    prompt_compact: bool,
    announcement: Option<&'a xai_grok_announcements::RemoteAnnouncement>,
    /// Whether a long announcement is expanded inline (vs. collapsed to 2 lines).
    expanded: bool,
    /// Whether the info slot reserves a promo upgrade CTA (spacer and button).
    has_upgrade_cta: bool,
    /// Rows reserved for the prompt box.
    /// `None` keeps the default; the blocking screens that paint no prompt pass 0 to give the rows back to their message.
    prompt_height: Option<u16>,
}

impl WelcomeLayout {
    /// Whether the hero box (side-by-side logo and menu inside a border) is active.
    pub(super) fn has_hero_box(&self) -> bool {
        self.hero_box.width > 0 && self.hero_box.height > 0
    }

    pub(super) fn fixed_below(tip_height: u16, prompt_height: u16) -> u16 {
        let tip_gap = if tip_height > 0 { 1u16 } else { 0 };
        tip_height + tip_gap + prompt_height + VERSION_GAP + 1
    }

    pub(super) fn effective_changelog(
        content_height: u16,
        fixed_above: u16,
        content_slot: u16,
        fixed_below: u16,
        requested: u16,
    ) -> (u16, u16) {
        let gap = if requested > 0 { 1u16 } else { 0 };
        let min_without = fixed_above + content_slot + 1 + fixed_below;
        if requested > 0 && content_height >= min_without + gap + requested {
            (requested, 1)
        } else {
            (0, 0)
        }
    }

    /// Compute the welcome screen layout, allowing the wide hero-box variant.
    /// The home: cat, wordmark, menu, downward.
    ///
    /// The mascot is 30 rows tall, so the screen is a column rather than a wide
    /// box with the art beside the menu — the art is the point, and the menu
    /// reads fine under it.
    fn compute(input: WelcomeLayoutInput<'_>) -> Self {
        Self::compute_inner(input, false)
    }

    /// That renderer only paints the stacked `logo`/`menu` rects (and never an announcement). The
    /// hero-box layout zeroes those, so the blocked path must stay stacked regardless of terminal size.
    fn compute_stacked(input: WelcomeLayoutInput<'_>) -> Self {
        Self::compute_inner(input, false)
    }

    /// Width depends only on content size, so the two phases cannot disagree. `allow_hero_box` gates
    /// the wide variant; stacked-only callers pass `false`.
    fn compute_inner(input: WelcomeLayoutInput<'_>, allow_hero_box: bool) -> Self {
        if allow_hero_box
            && !input.compact
            && input.content_area.width >= HERO_BOX_MIN_WIDTH
            && input.menu_height > 0
            && let Some(layout) = hero_box::compute_hero_box(&input)
        {
            return layout;
        }

        let WelcomeLayoutInput {
            content_area,
            error_height,
            menu_height,
            tip_height,
            changelog_height,
            compact,
            announcement,
            prompt_height,
            ..
        } = input;
        let zero = Rect::default();
        let prompt_height = prompt_height.unwrap_or(PROMPT_HEIGHT);
        // Centering and the info budget see the one-line box, so a growing draft does not shift the column or reflow the slot
        // The consent screen passes 0 rows and must not be charged for a box it never paints
        let one_line_prompt = prompt_height.min(PROMPT_HEIGHT);

        let gap_after_logo = if error_height > 0 { 1 } else { 0 };
        let tip_gap = if tip_height > 0 { 1u16 } else { 0 };
        let fixed_below = Self::fixed_below(tip_height, prompt_height);
        let column = StackedColumn::new(&input, prompt_height);

        // Stacked info slot: the announcement at its draft-independent rows, else the changelog
        // The announcement outranks the logo, so the tier steps down to keep it on screen; the changelog yields to the logo as before
        let announcement_rows = stacked_announcement_rows(&input);
        let info_height = if announcement.is_some() {
            announcement_rows
        } else {
            changelog_height
        };
        let reserved = if announcement_rows > 0 {
            announcement_rows + 1
        } else {
            0
        };
        // Stacked layout: skip the logo in compact mode (the session picker needs the space); otherwise the height picks the tier and only an overflowing column steps it down
        let logo_tier = if compact {
            LogoTier::Hidden
        } else {
            column.logo_tier(reserved)
        };
        let fixed_above = column.fixed_above(logo_tier);

        // The stacked info slot below the menu holds whichever block is shown (announcement or changelog), matching the hero box's single-slot rule
        let (eff_changelog_height, _) = if !compact {
            Self::effective_changelog(
                content_area.height,
                fixed_above,
                menu_height,
                fixed_below,
                info_height,
            )
        } else {
            (0, 0)
        };
        let eff_changelog_gap = if eff_changelog_height > 0 { 1u16 } else { 0 };
        let logo_gap = 1u16;
        let flex_gap = 1u16;
        // Compute top_pad using the *default* menu height (4 items, 7 rows) and the one-line prompt so the logo position stays constant regardless of picker/focus state or draft length
        let top_pad = if compact {
            0
        } else {
            let default_menu_height = 4u16;
            let remaining = content_area.height.saturating_sub(fixed_above);
            let centered = remaining
                .saturating_sub(default_menu_height)
                .saturating_sub(eff_changelog_gap + eff_changelog_height)
                .saturating_sub(Self::fixed_below(tip_height, one_line_prompt))
                / 3;
            // A growing draft consumes the flex gap before the padding
            centered.min(content_area.height.saturating_sub(
                fixed_above
                    + menu_height
                    + eff_changelog_gap
                    + eff_changelog_height
                    + flex_gap
                    + fixed_below,
            ))
        };
        let [
            _,
            logo,
            _,
            wordmark,
            _,
            error,
            menu,
            _,
            changelog,
            _,
            tip,
            _,
            prompt,
            _,
            version,
        ] = Layout::vertical([
            Constraint::Length(top_pad),
            Constraint::Length(logo_tier.rows()),
            Constraint::Length(logo_gap), // gap after logo
            Constraint::Length(WORDMARK_ROWS),
            Constraint::Length(gap_after_logo),
            Constraint::Length(error_height),
            Constraint::Length(menu_height),
            Constraint::Length(eff_changelog_gap),
            Constraint::Length(eff_changelog_height),
            Constraint::Min(flex_gap),
            Constraint::Length(tip_height),
            Constraint::Length(tip_gap),
            Constraint::Length(prompt_height),
            Constraint::Length(VERSION_GAP),
            // The badge moved up under the cat, so this row is the gap above
            // the bottom line (where the tip or a pending hint lands).
            Constraint::Length(0),
        ])
        .areas(content_area);
        Self {
            logo,
            wordmark,
            error,
            menu,
            changelog,
            tip,
            prompt,
            version,
            hero_box: zero,
            hero_logo: zero,
            hero_version: zero,
            hero_subtitle: zero,
            hero_info: zero,
            hero_menu: zero,
            logo_tier,
        }
    }
}

/// Controls what the version badge renders.
pub(super) enum VersionBadgeMode<'a> {
    /// Full badge: team | tier | api_key | **Remote-Code** VERSION+channel (right-aligned).
    Full { subscription_tier: Option<&'a str> },
    /// Hero footer: team | api_key | channel (right-aligned, gray).
    HeroFooter,
    /// Hero inline: **Remote-Code**  VERSION (left-aligned).
    HeroInline,
}

pub(super) fn render_version_badge(
    version_rect: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    team_name: Option<&str>,
    h_margin: u16,
    is_api_key_auth: bool,
    mode: VersionBadgeMode<'_>,
    align: Alignment,
) {
    let version_area = Rect {
        width: version_rect.width.saturating_sub(h_margin),
        ..version_rect
    };
    const SEP: &str = "  \u{2502}  ";
    let sep = Span::styled(
        SEP,
        Style::default().fg(theme.gray).add_modifier(Modifier::DIM),
    );
    let mut spans = Vec::new();

    // The mode decides what the badge says; the caller decides where it sits,
    // because the home reads as a left-aligned column.
    let (show_team, show_tier, show_api_key) = match &mode {
        VersionBadgeMode::Full { .. } => (true, true, true),
        VersionBadgeMode::HeroFooter => (true, false, true),
        VersionBadgeMode::HeroInline => (false, false, false),
    };

    if show_team && let Some(team) = team_name {
        spans.push(Span::styled(team, Style::default().fg(theme.gray)));
        spans.push(sep.clone());
    }
    if show_tier
        && let VersionBadgeMode::Full {
            subscription_tier: Some(tier),
        } = &mode
    {
        spans.push(Span::styled(
            format!("Tier: {tier}"),
            Style::default().fg(theme.gray),
        ));
        spans.push(sep.clone());
    }
    if show_api_key && is_api_key_auth {
        spans.push(Span::styled(
            "Logged in with API key",
            Style::default().fg(theme.gray),
        ));
        spans.push(sep);
    }

    let channel = xai_grok_update::channel_label();
    match &mode {
        VersionBadgeMode::Full { .. } => {
            spans.extend(title_spans(WORDMARK, theme));
            spans.push(Span::styled(
                format!("{}{}", xai_grok_version::VERSION, channel),
                Style::default().fg(theme.gray),
            ));
        }
        VersionBadgeMode::HeroFooter => {
            if !channel.is_empty() {
                spans.push(Span::styled(
                    channel.trim(),
                    Style::default().fg(theme.gray),
                ));
            }
        }
        VersionBadgeMode::HeroInline => {
            spans.extend(title_spans(WORDMARK, theme));
            spans.push(Span::styled(
                xai_grok_version::VERSION,
                Style::default().fg(theme.gray),
            ));
        }
    }

    // Only alpha and beta builds print a channel, so on stable the spans can end on a separator.
    if spans.last().is_some_and(|s| s.content == SEP) {
        spans.pop();
    }

    let version_line = Line::from(spans).alignment(align);
    Paragraph::new(version_line).render(version_area, buf);
}

/// Render the prompt box and version line (shared across welcome states). When `skip_version` is
/// true the version badge is not rendered (it was already drawn inside the hero box).
#[allow(clippy::too_many_arguments)]
fn render_prompt_and_version(
    layout: &WelcomeLayout,
    content_width: u16,
    buf: &mut Buffer,
    theme: &Theme,
    focus: WelcomePromptFocus,
    prompt: &mut PromptWidget,
    info: &PromptInfo<'_>,
    tip: Option<&str>,
    team_name: Option<&str>,
    h_margin: u16,
    compact: bool,
    pending_hint: Option<crate::views::shortcuts_bar::PendingHint>,
    is_api_key_auth: bool,
    skip_version: bool,
) -> (
    Option<(u16, u16)>,
    Option<crate::terminal::overlay::PostFlush>,
) {
    let [_, prompt_centered, _] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(content_width),
        Constraint::Min(0),
    ])
    .flex(Flex::Center)
    .areas(layout.prompt);

    if let Some(tip_text) = tip
        && layout.tip.height > 0
    {
        let [_, tip_centered, _] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(content_width),
            Constraint::Min(0),
        ])
        .flex(Flex::Center)
        .areas(layout.tip);
        let inset = prompt::prompt_inset(compact);
        let tip_inset = Rect {
            x: tip_centered.x + inset,
            y: tip_centered.y,
            width: tip_centered.width.saturating_sub(inset * 2),
            height: tip_centered.height,
        };
        crate::tips::render::render_tip(tip_inset, buf, tip_text, crate::tips::render::HINT_INSET);
    }
    let prompt_result = prompt::render_prompt(prompt_centered, buf, focus, prompt, info, compact);

    if let Some(pending) = &pending_hint {
        render_pending_hint(layout.version, buf, theme, pending);
    } else if !skip_version {
        render_version_badge(
            layout.version,
            buf,
            theme,
            team_name,
            h_margin,
            is_api_key_auth,
            VersionBadgeMode::Full {
                subscription_tier: None,
            },
            Alignment::Right,
        );
    } else {
        render_version_badge(
            layout.version,
            buf,
            theme,
            team_name,
            h_margin,
            is_api_key_auth,
            VersionBadgeMode::HeroFooter,
            Alignment::Right,
        );
    }

    prompt_result
}

/// All display state for rendering the welcome screen.
pub struct WelcomeRenderParams<'a> {
    pub prompt_focus: WelcomePromptFocus,
    pub auth_state: &'a AuthState,
    /// Folder-trust state.
    /// When `Pending` (auth done, access granted), the welcome screen renders the trust question instead of the normal prompt.
    pub trust_state: &'a TrustState,
    pub consent_state: &'a crate::app::consent::ConsentState,
    pub consent_hover_link: Option<usize>,
    pub login_label: Option<&'a str>,
    pub auth_code_input: &'a str,
    pub auth_code_cursor_byte: usize,
    pub clipboard_delivery: Option<crate::clipboard::ClipboardDelivery>,
    pub show_raw_url: bool,
    pub announcement: Option<&'a xai_grok_announcements::RemoteAnnouncement>,
    pub tip: Option<&'a str>,
    pub model_name: &'a str,
    pub flags: &'a [PromptFlag<'a>],
    pub selected: Option<usize>,
    pub team_name: Option<&'a str>,
    pub has_access: bool,
    pub has_claude_import: bool,
    pub mouse_pos: Option<(u16, u16)>,
    pub is_zdr_blocked: bool,
    pub session_picker: Option<&'a [SessionPickerEntry]>,
    pub session_picker_loading: bool,
    pub compact: bool,
    pub pending_hint: Option<crate::views::shortcuts_bar::PendingHint>,
    pub startup_warnings: &'a [StartupWarning],
    pub pending_update_version: Option<&'a str>,
    /// Recent foreign session offered on ctrl+u, suppressed by a pending update.
    pub foreign_resume_hint: Option<&'a xai_grok_foreign_sessions::RecentForeignSession>,
    pub is_api_key_auth: bool,
    pub session_picker_content_results:
        Option<&'a [xai_grok_shell::extensions::session_search::SearchSessionHit]>,
    pub session_picker_content_loading: bool,
    /// The query the picker entries were server-fetched with (see [`crate::views::session_picker::effective_filter_query`]).
    pub session_picker_entries_query: Option<&'a str>,
    pub welcome_tick: u64,
    pub gate: Option<&'a xai_grok_login::GateInfo>,
    pub subscription_tier: Option<&'a str>,
    pub session_picker_grouped: bool,
    /// Source filter for the session picker.
    pub session_picker_source_filter: crate::views::session_picker::SourceFilter,
    pub session_picker_pending_delete: bool,
    /// Process-wide `--chat`: the picker lists backend conversations only, so the source filter and local deep search are hidden.
    pub chat_mode: bool,
    /// Live working directory (tracks `Effect::SetWorkingDir`), used to pin the current repo's session group to the top of the picker.
    pub cwd: &'a std::path::Path,
    /// App-level credit balance for showing the usage warning on the welcome screen.
    pub credit_balance: Option<&'a crate::views::credit_bar::CreditBalance>,
    /// Auto top-up rule paired with `credit_balance` for the welcome warning.
    pub auto_topup: Option<&'a crate::views::credit_bar::AutoTopupInfo>,
    /// Whether the consumer billing UI applies (false for team / API-key, which get no credit warning).
    pub usage_visible: bool,
    /// Cached changelog bullets for the welcome screen (up to 3).
    pub changelog_bullets: &'a [String],
    /// Whether full release notes markdown is available (controls the CTA hint).
    pub changelog_has_full_notes: bool,
    /// Whether a long managed-config announcement is expanded inline (vs the default 2-line collapsed view with a trailing `…`).
    pub welcome_announcement_expanded: bool,
    /// Promo upgrade CTA `[label]` to paint below the hero announcement: `Some` drives both the reserved row height and the `[label]` button.
    /// `None` means no CTA on the welcome screen.
    pub upgrade_cta: Option<&'a str>,
    /// Non-blocking welcome privacy banner above the prompt.
    pub privacy_banner: bool,
    /// Chat-mode workspace picker selection (`local-workspace` feature).
    #[cfg(feature = "local-workspace")]
    pub workspace_mode: WelcomeWorkspaceMode,
    /// CLI/env already stamped the local workspace, so the picker is display-only.
    #[cfg(feature = "local-workspace")]
    pub workspace_mode_startup_locked: bool,
    /// In-TUI ACK confirm pending for Local.
    #[cfg(feature = "local-workspace")]
    pub workspace_mode_ack_pending: bool,
}

/// Render the welcome screen.
pub fn render_welcome(
    area: Rect,
    buf: &mut Buffer,
    params: &WelcomeRenderParams<'_>,
    prompt: &mut PromptWidget,
    session_picker_state: &mut crate::views::picker::PickerState,
) -> WelcomeRenderResult {
    let theme = Theme::current();
    let h_margin = if params.compact {
        H_MARGIN_COMPACT
    } else {
        H_MARGIN
    };
    let v_margin = 1u16;

    buf.set_style(area, Style::default().bg(theme.bg_base));

    // Announcements only render inside the hero box. Top bar is always 1 row.
    let [_, top_bar_area, content_area, _] = Layout::vertical([
        Constraint::Length(v_margin),
        Constraint::Length(1),
        Constraint::Min(10),
        Constraint::Length(v_margin),
    ])
    .areas(area);

    let top_bar_inner = Rect {
        x: top_bar_area.x + h_margin,
        y: top_bar_area.y,
        width: top_bar_area.width.saturating_sub(h_margin * 2),
        height: 1,
    };
    render_top_bar(top_bar_inner, buf, &theme, None);

    let mut result = match params.auth_state {
        AuthState::Pending { error } => {
            let label = params.login_label.unwrap_or("grok.com");
            let login_text = format!("Login with {}", label);
            let menu = [("l", login_text.as_str()), ("q", "Quit")];
            let msg = error.as_deref().map(|e| (e, theme.accent_error));
            let info = PromptInfo {
                model_name: params.model_name,
                flags: params.flags,
                multiline: false,
                usage_warning: None,
                usage_warning_critical: false,
            };
            let (menu_rects, post_flush_escapes) = render_welcome_blocked(
                content_area,
                buf,
                msg,
                &menu,
                params.selected,
                Some((prompt, &info)),
                h_margin,
                params.compact,
            );
            WelcomeRenderResult {
                cursor_pos: None,
                post_flush_escapes,
                menu_rects,
                ..Default::default()
            }
        }
        AuthState::Authenticating { auth_url, mode, .. } => {
            let llc = logo_line_count(content_area.height);
            let (url_rect, fallback_rect) = render_welcome_authenticating(
                content_area,
                buf,
                &theme,
                llc,
                auth_url.as_deref(),
                *mode,
                params.auth_code_input,
                params.auth_code_cursor_byte,
                params.clipboard_delivery,
                params.show_raw_url,
            );
            WelcomeRenderResult {
                auth_url_rect: url_rect,
                auth_fallback_rect: fallback_rect,
                ..Default::default()
            }
        }
        AuthState::Done if params.is_zdr_blocked => {
            let menu = [("l", "Switch account"), ("q", "Quit")];
            let (menu_rects, post_flush_escapes) = render_welcome_blocked(
                content_area,
                buf,
                Some((
                    "Remote-Code is not yet available for this account.",
                    theme.gray_bright,
                )),
                &menu,
                params.selected,
                None,
                h_margin,
                params.compact,
            );
            WelcomeRenderResult {
                post_flush_escapes,
                menu_rects,
                ..Default::default()
            }
        }
        // The `if let` destructure makes the `Pending`-only render structurally exhaustive (no
        // `unreachable!`).
        AuthState::Done if params.has_access => {
            // Consent is account-level, so it resolves before the workspace-level trust question.
            if let ConsentState::Pending { notice, .. } = params.consent_state {
                consent::render_consent(
                    content_area,
                    buf,
                    &theme,
                    notice,
                    params.selected,
                    params.consent_hover_link,
                    params.pending_hint,
                    h_margin,
                    params.compact,
                )
            } else if let TrustState::Pending { workspace } = params.trust_state {
                render_welcome_trust(
                    content_area,
                    buf,
                    &theme,
                    workspace,
                    params.selected,
                    h_margin,
                    params.compact,
                )
            } else {
                render_welcome_done(
                    content_area,
                    buf,
                    &theme,
                    params,
                    prompt,
                    session_picker_state,
                    h_margin,
                )
            }
        }
        AuthState::Done => render_welcome_done(
            content_area,
            buf,
            &theme,
            params,
            prompt,
            session_picker_state,
            h_margin,
        ),
    };
    if result.post_flush_escapes.is_none() {
        result.post_flush_escapes = crate::terminal::overlay::clear().map(Into::into);
    }
    result
}

/// Render a blocked welcome screen: logo, optional message, menu, version. Used for both the login
/// screen (Pending) and the ZDR gate. The layout is: Logo. {prompt} (optional). Version badge.
#[allow(clippy::too_many_arguments)]
fn render_welcome_blocked(
    content_area: Rect,
    buf: &mut Buffer,
    message: Option<(&str, ratatui::style::Color)>,
    menu_items: &[(&str, &str)],
    selected: Option<usize>,
    prompt: Option<(&mut PromptWidget, &PromptInfo<'_>)>,
    h_margin: u16,
    compact: bool,
) -> (Vec<Rect>, Option<crate::terminal::overlay::PostFlush>) {
    let theme = Theme::current();

    let msg_height = if message.is_some() { 2u16 } else { 0u16 };
    let menu_height = menu_items.len() as u16;
    let mut layout_input = WelcomeLayoutInput {
        content_area,
        error_height: msg_height,
        menu_height,
        compact,
        prompt_compact: compact,
        ..Default::default()
    };
    // The login screen paints the home draft (logout keeps it), so it measures the box the same way home does
    if let Some((prompt_widget, _)) = prompt.as_ref() {
        layout_input.prompt_height = Some(prompt::desired_prompt_height(
            prompt_widget,
            content_area.width,
            compact,
            prompt_max_height(&layout_input),
        ));
    }
    // Force the stacked layout: this renderer only paints the stacked logo/menu rects, which the hero-box layout would leave empty
    let layout = WelcomeLayout::compute_stacked(layout_input);

    render_logo_tier(layout.logo, buf, &theme, layout.logo_tier);

    if let Some((text, color)) = message {
        // Left, with the rest of the home column.
        let line = Line::from(Span::styled(text, Style::default().fg(color)));
        Paragraph::new(line).render(layout.error, buf);
    }

    // Inset the menu the same as the input bar / post-auth menu
    // The actions keep side spacing instead of touching the window edge on narrow terminals
    let menu_area = inset_horizontal(layout.menu, prompt::prompt_inset(compact));
    let menu_rects = render_menu(
        menu_area,
        buf,
        &theme,
        menu_items,
        selected,
        None,
        0,
        Alignment::Center,
    );

    let post_flush_escapes = if let Some((prompt_widget, info)) = prompt {
        let [_, prompt_centered, _] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(content_area.width),
            Constraint::Min(0),
        ])
        .flex(Flex::Center)
        .areas(layout.prompt);
        prompt::render_prompt(
            prompt_centered,
            buf,
            WelcomePromptFocus::Unfocused,
            prompt_widget,
            info,
            compact,
        )
        .1
    } else {
        None
    };

    render_version_badge(
        layout.version,
        buf,
        &theme,
        None,
        h_margin,
        false,
        VersionBadgeMode::Full {
            subscription_tier: None,
        },
        Alignment::Right,
    );
    (menu_rects, post_flush_escapes)
}

/// The y/N answer is handled by the welcome input interceptor, so this only paints; `menu_rects`
/// are returned for parity with the other welcome arms.
fn render_welcome_trust(
    content_area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    workspace: &std::path::Path,
    selected: Option<usize>,
    h_margin: u16,
    compact: bool,
) -> WelcomeRenderResult {
    let menu_items = [("y", "Yes, proceed"), ("n", "No, quit")];
    let lines = vec![
        Line::from(Span::styled(
            "Do you trust the contents of this directory?",
            Style::default().fg(theme.gray_bright),
        ))
        .alignment(Alignment::Center),
        Line::from(Span::styled(
            workspace.display().to_string(),
            Style::default().fg(theme.accent_user),
        ))
        .alignment(Alignment::Center),
        Line::default(),
        // Two lines so the warning never clips at narrow / compact widths (a single ~78-char line would truncate "...posing security risks")
        Line::from(Span::styled(
            "Remote-Code may run or modify contents in this directory,",
            Style::default().fg(theme.gray),
        ))
        .alignment(Alignment::Center),
        Line::from(Span::styled(
            "posing security risks.",
            Style::default().fg(theme.gray),
        ))
        .alignment(Alignment::Center),
        // Spacer between the warning and the y/n menu.
        Line::default(),
    ];

    let msg_height = lines.len() as u16;
    let menu_height = menu_items.len() as u16;
    let layout = WelcomeLayout::compute_stacked(WelcomeLayoutInput {
        content_area,
        error_height: msg_height,
        menu_height,
        compact,
        prompt_compact: compact,
        ..Default::default()
    });

    render_logo_tier(layout.logo, buf, theme, layout.logo_tier);
    Paragraph::new(lines).render(layout.error, buf);

    let menu_area = inset_horizontal(layout.menu, prompt::prompt_inset(compact));
    let menu_rects = render_menu(
        menu_area,
        buf,
        theme,
        &menu_items,
        selected,
        None,
        0,
        Alignment::Center,
    );

    render_version_badge(
        layout.version,
        buf,
        theme,
        None,
        h_margin,
        false,
        VersionBadgeMode::Full {
            subscription_tier: None,
        },
        Alignment::Right,
    );

    // Only `menu_rects` are meaningful here; the rest are absent (no prompt, picker, auth/gate links)
    // `Default` keeps this honest without a 13-field all-`None` literal
    WelcomeRenderResult {
        menu_rects,
        ..Default::default()
    }
}

/// Header text shared by Loopback and Command auth modes.
const AUTH_HEADER: &str = "A browser window will open for authentication.";
/// Header text for the device-flow auth mode.
const DEVICE_AUTH_HEADER: &str = "Approve in your browser to finish signing in.";
/// Caption beneath the device code.
const DEVICE_CODE_CAPTION: &str = "Make sure your browser shows this code.";

/// Extract `user_code` from a device verification URL (`None` if absent or malformed).
/// It is shown on-screen so the user can confirm it matches the browser before approving (anti-phishing).
fn extract_user_code(url: &str) -> Option<&str> {
    let code = url
        .split('?')
        .nth(1)?
        .split('&')
        .find_map(|kv| kv.strip_prefix("user_code="))?;
    let valid = !code.is_empty() && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    valid.then_some(code)
}
/// Clickable copy prompt shared by Loopback and Command auth modes.
const AUTH_COPY_PREFIX: &str = "If it doesn't open, click ";
const AUTH_COPY_HERE: &str = "here";
const AUTH_COPY_SUFFIX: &str = " to copy.";

/// Build the "click here to copy" line with "here" underlined in accent color.
fn auth_copy_line(theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(AUTH_COPY_PREFIX, Style::default().fg(theme.gray_bright)),
        Span::styled(
            AUTH_COPY_HERE,
            Style::default()
                .fg(theme.accent_user)
                .add_modifier(Modifier::UNDERLINED),
        ),
        Span::styled(AUTH_COPY_SUFFIX, Style::default().fg(theme.gray_bright)),
    ])
    .alignment(Alignment::Center)
}

/// Number of physical rows the header and the blank row occupy before the copy line.
fn auth_copy_preceding_rows(header: &str, inner_width: u16) -> u16 {
    let header_rows = (header.len() as u16).div_ceil(inner_width);
    header_rows + 1 // header + blank
}

/// Number of physical rows the copy line occupies when wrapped.
fn auth_copy_line_rows(inner_width: u16) -> u16 {
    let copy_len = AUTH_COPY_PREFIX.len() + AUTH_COPY_HERE.len() + AUTH_COPY_SUFFIX.len();
    (copy_len as u16).div_ceil(inner_width)
}

const AUTH_FALLBACK_TEXT: &str = "Copying not working? Click here to show full URL.";

/// Build the fallback "show full URL" link line.
fn auth_fallback_line(theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        AUTH_FALLBACK_TEXT,
        Style::default()
            .fg(theme.gray)
            .add_modifier(Modifier::UNDERLINED),
    ))
    .alignment(Alignment::Center)
}

/// Push the shared copy-prompt block, stable feedback slot, and raw-URL fallback.
fn push_auth_copy_block(
    lines: &mut Vec<Line<'static>>,
    theme: &Theme,
    clipboard_delivery: Option<crate::clipboard::ClipboardDelivery>,
) {
    lines.push(Line::default());
    lines.push(auth_copy_line(theme));
    lines.push(Line::default());
    lines.push(match clipboard_delivery {
        Some(crate::clipboard::ClipboardDelivery::Confirmed) => {
            Line::from(Span::styled("copied!", Style::default().fg(theme.gray)))
                .alignment(Alignment::Center)
        }
        Some(crate::clipboard::ClipboardDelivery::Unverified) => Line::from(Span::styled(
            "copy sent: verify paste",
            Style::default().fg(theme.gray),
        ))
        .alignment(Alignment::Center),
        Some(crate::clipboard::ClipboardDelivery::Failed) => {
            Line::from(Span::styled("copy failed", Style::default().fg(theme.gray)))
                .alignment(Alignment::Center)
        }
        None => Line::default(),
    });
    lines.push(Line::default());
    lines.push(auth_fallback_line(theme));
}

/// Rows occupied by [`push_auth_copy_block`].
fn auth_copy_block_rows(inner_width: u16) -> u16 {
    auth_copy_line_rows(inner_width) + 5
}

/// Click hit-rects for the copy line and fallback link. `header`'s wrapped row count sets the copy line's vertical offset.
fn auth_hit_rects(
    msg_area: Rect,
    h_pad: u16,
    inner_width: u16,
    header: &str,
    preceding_extra: u16,
) -> (Option<Rect>, Option<Rect>) {
    let preceding = auth_copy_preceding_rows(header, inner_width) + preceding_extra;
    let copy_rows = auth_copy_line_rows(inner_width);
    let copy_rect = Rect {
        x: msg_area.x + h_pad,
        y: msg_area.y + preceding,
        width: inner_width,
        height: copy_rows,
    };
    // The fallback line is after: copy_rows + blank + copied_slot + blank
    let fallback_y = msg_area.y + preceding + copy_rows + 3;
    let fb_rect = Rect {
        x: msg_area.x + h_pad,
        y: fallback_y,
        width: inner_width,
        height: 1,
    };
    (Some(copy_rect), Some(fb_rect))
}

/// Render the "raw URL" mode: shows the full URL with mouse capture disabled so the user can select and copy it natively.
fn render_raw_url_mode(
    content_area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    top_pad: u16,
    logo_line_count: u16,
    auth_url: Option<&str>,
) -> (Option<Rect>, Option<Rect>) {
    // Use full terminal width for the URL so the terminal wraps it naturally without inserting spaces (important for copy-paste)
    let full_width = content_area.width.max(1);
    let url_lines = auth_url
        .map(|u| (u.len() as u16).div_ceil(full_width))
        .unwrap_or(0);
    let msg_height = 1 + 1 + url_lines; // hint + blank + URL
    let [_, logo_area, _, msg_area, _, hint_area, _] = Layout::vertical([
        Constraint::Length(top_pad),
        Constraint::Length(logo_line_count),
        Constraint::Length(2),
        Constraint::Length(msg_height),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(content_area);

    render_logo(logo_area, buf, theme, content_area.height);

    // Render hint above the URL.
    let hint = Line::from(Span::styled(
        "Select the URL below with your mouse and copy manually.",
        Style::default().fg(theme.gray),
    ))
    .alignment(Alignment::Center);
    Paragraph::new(hint).render(
        Rect {
            height: 1,
            ..msg_area
        },
        buf,
    );

    // Write the URL directly to the buffer character-by-character so the terminal wraps naturally at
    // the screen edge.
    if let Some(url) = auth_url {
        let url_style = Style::default().fg(theme.accent_user);
        let url_y = msg_area.y + 2; // after hint + blank
        // Control characters are skipped below to prevent terminal escape injection, so measure the URL without them
        let url_len = url.chars().filter(|c| !c.is_control()).count() as u16;
        let x_offset = if url_len <= full_width {
            (full_width - url_len) / 2
        } else {
            0
        };
        let buf_area = buf.area();
        let buf_max_col = buf_area.x + buf_area.width;
        let buf_max_row = buf_area.y + buf_area.height;
        for (i, ch) in url.chars().filter(|c| !c.is_control()).enumerate() {
            let col = msg_area.x + x_offset + (i as u16) % full_width;
            let row = url_y + (i as u16) / full_width;
            if row >= msg_area.y + msg_area.height {
                break;
            }
            // Guard against OOB access during resize races.
            if col >= buf_max_col || row >= buf_max_row {
                continue;
            }
            if let Some(cell) = buf.cell_mut((col, row)) {
                cell.set_char(ch).set_style(url_style);
            }
        }
    }

    let hint_spans = vec![
        Span::styled(
            "ctrl+q",
            Style::default()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  go back", Style::default().fg(theme.gray)),
    ];
    let hints = Line::from(hint_spans).alignment(Alignment::Center);
    Paragraph::new(hints).render(hint_area, buf);

    (None, None) // no click rects; mouse capture is disabled
}

/// Which "browser opened, now waiting" arm to render; owns the header, waiting caption, and (for `Device`) the device-code derivation.
#[derive(Clone, Copy)]
enum BrowserStatusKind {
    /// External auth provider opened its own browser.
    Command,
    /// RFC 8628 device flow; also shows the device code.
    Device,
}

/// Render a "browser opened, now waiting" auth arm (Command and Device). Shared status layout:
/// logo, then a centered block, then quit hints. The block holds the header, an optional device
/// code and caption, optional copy/fallback links (when there's a URL), and the waiting caption.
#[allow(clippy::too_many_arguments)]
fn render_browser_status_arm(
    content_area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    top_pad: u16,
    logo_line_count: u16,
    auth_url: Option<&str>,
    show_raw_url: bool,
    clipboard_delivery: Option<crate::clipboard::ClipboardDelivery>,
    kind: BrowserStatusKind,
) -> (Option<Rect>, Option<Rect>) {
    let h_pad: u16 = content_area.width / 6;
    let inner_width = content_area.width.saturating_sub(h_pad * 2).max(1);

    if show_raw_url {
        return render_raw_url_mode(content_area, buf, theme, top_pad, logo_line_count, auth_url);
    }

    // Device also parses the user code from the verification URL.
    let (header, waiting_text, user_code) = match kind {
        BrowserStatusKind::Command => (AUTH_HEADER, "Waiting for login to complete...", None),
        BrowserStatusKind::Device => (
            DEVICE_AUTH_HEADER,
            "Waiting for approval...",
            auth_url.and_then(extract_user_code),
        ),
    };

    let header_rows = (header.len() as u16).div_ceil(inner_width);
    let code_extra = if user_code.is_some() {
        let caption_rows = (DEVICE_CODE_CAPTION.len() as u16).div_ceil(inner_width);
        1 + 1 + 1 + caption_rows // blank + code + blank + caption
    } else {
        0
    };
    let copy_extra = if auth_url.is_some() {
        auth_copy_block_rows(inner_width)
    } else {
        0
    };
    let msg_height = header_rows + code_extra + copy_extra + 1 + 1; // blank + waiting

    let [_, logo_area, _, msg_area, _, hint_area, _] = Layout::vertical([
        Constraint::Length(top_pad),
        Constraint::Length(logo_line_count),
        Constraint::Length(2),          // gap
        Constraint::Length(msg_height), // status message
        Constraint::Min(1),             // gap
        Constraint::Length(1),          // hints
        Constraint::Min(0),
    ])
    .areas(content_area);

    render_logo(logo_area, buf, theme, content_area.height);

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(header, Style::default().fg(theme.gray_bright)))
            .alignment(Alignment::Center),
    ];
    if let Some(code) = user_code {
        lines.push(Line::default());
        lines.push(
            Line::from(Span::styled(
                code.to_owned(),
                Style::default()
                    .fg(theme.text_primary)
                    .add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Center),
        );
        lines.push(Line::default());
        lines.push(
            Line::from(Span::styled(
                DEVICE_CODE_CAPTION,
                Style::default().fg(theme.gray),
            ))
            .alignment(Alignment::Center),
        );
    }
    if auth_url.is_some() {
        push_auth_copy_block(&mut lines, theme, clipboard_delivery);
    }
    lines.push(Line::default());
    lines.push(
        Line::from(Span::styled(waiting_text, Style::default().fg(theme.gray)))
            .alignment(Alignment::Center),
    );
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().padding(Padding::horizontal(h_pad)))
        .render(msg_area, buf);

    let (click_rect, fallback_rect) = if auth_url.is_some() {
        auth_hit_rects(msg_area, h_pad, inner_width, header, code_extra)
    } else {
        (None, None)
    };

    let hints = Line::from(quit_hint_spans(theme)).alignment(Alignment::Center);
    Paragraph::new(hints).render(hint_area, buf);

    (click_rect, fallback_rect)
}

/// Render the welcome screen during authentication (Authenticating state).
#[allow(clippy::too_many_arguments)]
fn render_welcome_authenticating(
    content_area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    logo_line_count: u16,
    auth_url: Option<&str>,
    mode: AuthMode,
    auth_code_input: &str,
    auth_code_cursor_byte: usize,
    clipboard_delivery: Option<crate::clipboard::ClipboardDelivery>,
    show_raw_url: bool,
) -> (Option<Rect>, Option<Rect>) {
    let top_pad = content_area.height.saturating_sub(logo_line_count) / 10;

    match mode {
        AuthMode::Loopback => {
            // Manual token paste: show copy prompt and input box
            let h_pad: u16 = content_area.width / 6;
            let inner_width = content_area.width.saturating_sub(h_pad * 2).max(1);

            if show_raw_url {
                return render_raw_url_mode(
                    content_area,
                    buf,
                    theme,
                    top_pad,
                    logo_line_count,
                    auth_url,
                );
            }

            let msg_height = if auth_url.is_some() {
                let header_rows = (AUTH_HEADER.len() as u16).div_ceil(inner_width);
                header_rows + auth_copy_block_rows(inner_width)
            } else {
                1u16
            };
            let [_, logo_area, _, msg_area, _, prompt_area, _, hint_area, _] = Layout::vertical([
                Constraint::Length(top_pad),
                Constraint::Length(logo_line_count),
                Constraint::Length(1),          // gap
                Constraint::Length(msg_height), // instruction + copy prompt
                Constraint::Min(1),             // gap
                Constraint::Length(5),          // prompt box
                Constraint::Length(1),          // gap
                Constraint::Length(1),          // hints
                Constraint::Min(0),
            ])
            .areas(content_area);

            render_logo(logo_area, buf, theme, content_area.height);

            // Instruction text
            let mut lines: Vec<Line> = Vec::new();
            if auth_url.is_some() {
                lines.push(
                    Line::from(Span::styled(
                        AUTH_HEADER,
                        Style::default().fg(theme.gray_bright),
                    ))
                    .alignment(Alignment::Center),
                );
                push_auth_copy_block(&mut lines, theme, clipboard_delivery);
            } else {
                lines.push(
                    Line::from(Span::styled(
                        "Waiting for auth URL...",
                        Style::default().fg(theme.gray),
                    ))
                    .alignment(Alignment::Center),
                );
            }
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::default().padding(Padding::horizontal(h_pad)))
                .render(msg_area, buf);

            let (click_rect, fallback_rect) = if auth_url.is_some() {
                auth_hit_rects(msg_area, h_pad, inner_width, AUTH_HEADER, 0)
            } else {
                (None, None)
            };

            // Prompt box with token input
            let prompt_width = content_area.width;
            let [_, prompt_centered, _] = Layout::horizontal([
                Constraint::Min(0),
                Constraint::Length(prompt_width),
                Constraint::Min(0),
            ])
            .flex(Flex::Center)
            .areas(prompt_area);
            render_auth_input_box(
                prompt_centered,
                buf,
                theme,
                auth_code_input,
                auth_code_cursor_byte,
            );

            // Hints
            let mut hint_spans = vec![
                Span::styled(
                    "enter",
                    Style::default()
                        .fg(theme.accent_user)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  submit    ", Style::default().fg(theme.gray)),
            ];
            hint_spans.extend(quit_hint_spans(theme));
            let hints = Line::from(hint_spans).alignment(Alignment::Center);
            Paragraph::new(hints).render(hint_area, buf);

            (click_rect, fallback_rect)
        }

        AuthMode::Command => render_browser_status_arm(
            content_area,
            buf,
            theme,
            top_pad,
            logo_line_count,
            auth_url,
            show_raw_url,
            clipboard_delivery,
            BrowserStatusKind::Command,
        ),

        AuthMode::Device => render_browser_status_arm(
            content_area,
            buf,
            theme,
            top_pad,
            logo_line_count,
            auth_url,
            show_raw_url,
            clipboard_delivery,
            BrowserStatusKind::Device,
        ),

        AuthMode::Pending => {
            // Connecting: status text
            let [_, logo_area, _, msg_area, _, hint_area, _] = Layout::vertical([
                Constraint::Length(top_pad),
                Constraint::Length(logo_line_count),
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .areas(content_area);

            render_logo(logo_area, buf, theme, content_area.height);

            let msg = Line::from(Span::styled(
                "Connecting...",
                Style::default().fg(theme.gray_bright),
            ))
            .alignment(Alignment::Center);
            Paragraph::new(msg).render(msg_area, buf);

            let hints = Line::from(quit_hint_spans(theme)).alignment(Alignment::Center);
            Paragraph::new(hints).render(hint_area, buf);

            (None, None)
        }
    }
}

/// Shrink a rect by `inset` columns on the left and right (clamped at 0).
fn inset_horizontal(rect: Rect, inset: u16) -> Rect {
    Rect {
        x: rect.x + inset,
        width: rect.width.saturating_sub(inset * 2),
        ..rect
    }
}

/// Render the changelog section (header and bullets), centered to the menu width.
/// When `clickable` (full notes exist) the whole block opens the notes on click and brightens while hovered; returns that clickable rect.
#[allow(clippy::too_many_arguments)]
fn render_changelog_section(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    bullets: &[String],
    min_width_hint: u16,
    content_height: u16,
    clickable: bool,
    mouse_pos: Option<(u16, u16)>,
) -> Option<Rect> {
    let menu_width = logo::logo_visual_width(content_height)
        .max(30)
        .max(min_width_hint);
    let [_, centered, _] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(menu_width),
        Constraint::Min(0),
    ])
    .flex(Flex::Center)
    .areas(area);

    if centered.width < 20 || centered.height == 0 {
        return None;
    }

    let hovered =
        clickable && mouse_pos.is_some_and(|(mx, my)| centered.contains(Position::new(mx, my)));

    let header_style = hover_style(
        theme,
        hovered,
        Style::default()
            .fg(theme.gray_bright)
            .add_modifier(Modifier::DIM),
    );
    let title = "Changelog";
    buf.set_span(
        centered.x,
        centered.y,
        &Span::styled(title, header_style),
        centered.width,
    );

    let bullet_style = hover_style(theme, hovered, Style::default().fg(theme.gray_bright));
    let max_text_width = centered.width.saturating_sub(2) as usize; // The "• " prefix is 2 cols
    for (i, bullet) in bullets.iter().enumerate() {
        let row = centered.y + 2 + i as u16;
        if row >= centered.y + centered.height {
            break;
        }
        let truncated = crate::render::line_utils::truncate_str(bullet, max_text_width);
        let text = format!("\u{2022} {truncated}");
        buf.set_span(
            centered.x,
            row,
            &Span::styled(text, bullet_style),
            centered.width,
        );
    }

    clickable.then_some(centered)
}

/// Wrap width of the stacked info slot, centered at the menu width inside the inset.
/// Both `compute`'s height measurement and `render_announcement_section` go through here, so the widths cannot drift.
/// `logo_height` selects the min menu width.
fn stacked_info_width(avail_width: u16, logo_height: u16, min_width_hint: u16) -> u16 {
    logo::logo_visual_width(logo_height)
        .max(30)
        .max(min_width_hint)
        .min(avail_width)
}

/// Render the announcement in the stacked info slot, centered to the menu width.
/// Returns `(block_rect, truncated)`: the clickable block and the overflow flag.
#[allow(clippy::too_many_arguments)]
fn render_announcement_section(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    announcement: &xai_grok_announcements::RemoteAnnouncement,
    min_width_hint: u16,
    content_height: u16,
    expanded: bool,
    mouse_pos: Option<(u16, u16)>,
    upgrade_cta: Option<&str>,
) -> (Option<Rect>, bool, Option<Rect>) {
    // Same width the height pre-pass reserved for (see `stacked_info_width`).
    let menu_width = stacked_info_width(area.width, content_height, min_width_hint);
    let [_, centered, _] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(menu_width),
        Constraint::Min(0),
    ])
    .flex(Flex::Center)
    .areas(area);

    if centered.width < 20 || centered.height == 0 {
        return (None, false, None);
    }

    // Mirror the hero: reserve the CTA rows at the bottom, draw the text into what's left, then place the `[label]` button right after the drawn text
    let (text_area, truncated, cta_rect) = hero_box::render_announcement_with_upgrade_cta(
        buf,
        theme,
        centered,
        announcement,
        expanded,
        mouse_pos,
        upgrade_cta,
    );
    (Some(text_area), truncated, cta_rect)
}

/// Render the normal welcome screen (Done state, already authenticated).
fn render_welcome_done(
    content_area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    p: &WelcomeRenderParams<'_>,
    prompt: &mut PromptWidget,
    session_picker_state: &mut crate::views::picker::PickerState,
    h_margin: u16,
) -> WelcomeRenderResult {
    let show_picker = p.session_picker.is_some() || p.session_picker_loading;
    // Only use compact layout when the session picker is visible; it needs the logo/centering space for its list
    // Plain compact mode keeps the normal welcome layout
    let welcome_compact = show_picker;

    let cta = p
        .gate
        .and_then(|g| g.label.as_deref())
        .unwrap_or("Upgrade Subscription");
    let in_vscode_family = welcome_in_vscode_family();
    let (key_g, key_l, key_q) = (
        "ctrl+g",
        "ctrl+l",
        if in_vscode_family { "ctrl+d" } else { "ctrl+q" },
    );

    // Heights that don't depend on the menu, computed first so the menu builder can probe the layout to decide whether to add a Changelog row
    // Startup-warning hint height (multi-line aware). It must pick the same entry `render_startup_warnings` draws; see `startup::banner_warning`.
    let hint_height = crate::startup::banner_warning(p.startup_warnings).map_or(0u16, |w| {
        let msg_lines = w.message.lines().count() as u16;
        let action_line = if w.action.is_some() { 1 } else { 0 };
        msg_lines + action_line + 1 // +1 for buffer spacing
    });
    let has_update_tip = p.pending_update_version.is_some();
    let has_resume_tip = !has_update_tip && p.foreign_resume_hint.is_some();
    // Tip slot precedence: pending update, then privacy banner (wraps, so its height depends on width), then resume hint, then random tip
    // The update outranks the upsell so a ready update is never invisible; the banner takes the slot back once it's applied
    let tip_height = if !show_picker {
        if has_update_tip {
            1u16
        } else if p.privacy_banner {
            // Same inset the banner paint below uses, so the reserved rows and the wrapped row count can't drift
            let inset = prompt::prompt_inset(p.compact);
            crate::views::privacy_banner::height(content_area.width.saturating_sub(inset * 2))
        } else if has_resume_tip {
            1u16
        } else if let Some(tip_text) = p.tip {
            let inset = prompt::prompt_inset(welcome_compact);
            let tip_width = content_area.width.saturating_sub(inset * 2);
            crate::tips::render::tip_height(tip_width, tip_text)
        } else {
            0
        }
    } else {
        0
    };
    // The home keeps no changelog: the rows belong to the cat. The release notes
    // stay one command away (`/release-notes`), and the slot below the menu is
    // left to an announcement when the owner has them on.
    let changelog_height = 0;

    let gate_menu;
    let owned_menu;
    let menu_items: &[(&str, &str)] = if !p.has_access {
        // The provider rows are on this menu too: signing in to ChatGPT or
        // OpenRouter is exactly what a user without a Grok session needs, and the
        // harness runs on any of them — or on local models only.
        gate_menu = [
            (key_g, cta),
            ("", "Log in with ChatGPT"),
            ("", "Log in with OpenRouter"),
            (key_l, "Logout"),
            (key_q, "Quit"),
        ];
        &gate_menu
    } else {
        let (key_w, key_resume, key_q, key_i_with_x) = (
            "ctrl+w",
            "ctrl+r",
            if in_vscode_family { "ctrl+d" } else { "ctrl+q" },
            "ctrl+i  [x]",
        );
        // Insert the import row at the top when there are pending `.claude/` settings to import; it's the most actionable item right now
        let mut items: Vec<(&str, &str)> = Vec::with_capacity(9);
        if p.has_claude_import {
            // The trailing "[x]" is a clickable dismiss control
            // The welcome screen mouse handler treats clicks on the rightmost 3 cells of this row as dismiss instead of open. Keyboard: ctrl-shift-i.
            // The key string is right-aligned by render_menu, so [x] sits at the very end of the row
            items.push((key_i_with_x, "Import Claude settings"));
        }
        items.push((key_w, "New worktree"));
        items.push((key_resume, "Resume session"));
        // Provider rows: Remote-Code routes to more than one backend, so signing in
        // to each one, and picking the models a call may run on, belong in the menu.
        // No shortcut: the notice they open names the command to run inside a session.
        items.push(("", "Log in with Grok"));
        items.push(("", "Log in with ChatGPT"));
        items.push(("", "Log in with OpenRouter"));
        items.push(("", "Model tiers"));
        items.push((key_q, "Quit"));
        owned_menu = items;
        owned_menu.as_slice()
    };

    #[cfg(feature = "local-workspace")]
    // Keep the segmented control (and ACK y/N) visible when history is open if first-run Local ACK is pending
    // Otherwise the confirm is unpainted while the ACK handler still swallows keys
    let show_workspace_picker =
        p.chat_mode && p.has_access && (!show_picker || p.workspace_mode_ack_pending);
    #[cfg(feature = "local-workspace")]
    let workspace_picker_rows = if show_workspace_picker {
        workspace_mode::WORKSPACE_MODE_MENU_ROWS
    } else {
        0
    };
    #[cfg(not(feature = "local-workspace"))]
    let workspace_picker_rows = 0u16;

    let menu_height = if show_picker {
        0
    } else {
        menu_items.len() as u16 + workspace_picker_rows
    };

    // Session picker height: 1 row per entry (no dividers), scrollable.
    let picker_count = p.session_picker.map_or(0, |s| s.len());
    let picker_height = if show_picker {
        if p.session_picker_loading {
            1
        } else {
            // Reserve a row for the pinned hidden-external hint when shown.
            let hint_row = u16::from(
                !p.chat_mode
                    && crate::views::session_picker::hidden_external_hint(
                        p.session_picker,
                        p.session_picker_source_filter,
                    )
                    .is_some(),
            );
            (picker_count as u16).min(15) + 3 + hint_row // +3 for title + search + gap
        }
    } else {
        0
    };
    let content_height = menu_height + picker_height;
    // The layout measures the announcement slot itself
    // Collapsed is the title plus up to 2 wrapped lines; expanded is the full message, clamped so the box fits
    let mut layout_input = WelcomeLayoutInput {
        content_area,
        error_height: hint_height,
        menu_height: content_height,
        tip_height,
        changelog_height,
        compact: welcome_compact,
        prompt_compact: p.compact,
        announcement: p.announcement,
        expanded: p.welcome_announcement_expanded,
        has_upgrade_cta: p.upgrade_cta.is_some(),
        prompt_height: None,
    };
    // The picker and the access gate paint no composer
    if !show_picker && p.has_access {
        layout_input.prompt_height = Some(prompt::desired_prompt_height(
            prompt,
            content_area.width,
            p.compact,
            prompt_max_height(&layout_input),
        ));
    }
    let layout = WelcomeLayout::compute(layout_input);

    // Render startup warning in the error area (same slot as auth errors).
    let import_banner_rect = render_startup_warnings(layout.error, buf, theme, p.startup_warnings);

    // Hit-rects / truncation flag, set by whichever layout draws each block.
    let mut changelog_cta_rect: Option<Rect> = None;
    let mut announcement_truncated = false;
    let mut announcement_rect: Option<Rect> = None;
    let mut upgrade_cta_rect: Option<Rect> = None;

    #[cfg(feature = "local-workspace")]
    let mut workspace_mode_rects = WorkspaceModeHitRects::default();
    let (menu_rects, picker_close_button) = if show_picker {
        // Use the full area since logo/menu are hidden and shortcuts are now rendered inside the picker content area
        let picker_area = Rect {
            x: content_area.x,
            y: content_area.y,
            width: content_area.width,
            height: content_area.height,
        };
        let hit_areas = render_session_picker(
            picker_area,
            buf,
            theme,
            SessionPickerRenderMode::Fullscreen,
            &mut SessionPickerRenderCtx {
                state: session_picker_state,
                sessions: p.session_picker,
                loading: p.session_picker_loading,
                pending_hint: p.pending_hint,
                shortcuts_area: None,
                content_results: p.session_picker_content_results,
                content_loading: p.session_picker_content_loading,
                entries_query: p.session_picker_entries_query,
                tick: p.welcome_tick,
                grouped: p.session_picker_grouped,
                source_filter: p.session_picker_source_filter,
                pending_delete: p.session_picker_pending_delete,
                chat_mode: p.chat_mode,
                cwd: p.cwd,
            },
        );
        (vec![], Some(hit_areas))
    } else if layout.has_hero_box() {
        // Wide layout: render the bordered hero box with the logo left, version and menu right
        let rects = hero_box::render_hero_box(
            &layout,
            buf,
            theme,
            menu_items,
            p.selected,
            p.mouse_pos,
            p.announcement,
            p.welcome_announcement_expanded,
            p.changelog_bullets,
            p.changelog_has_full_notes,
            p.upgrade_cta,
            #[cfg(feature = "local-workspace")]
            show_workspace_picker.then_some((
                p.workspace_mode,
                p.workspace_mode_startup_locked,
                p.workspace_mode_ack_pending,
            )),
        );
        changelog_cta_rect = rects.changelog_cta_rect;
        announcement_truncated = rects.announcement_truncated;
        announcement_rect = rects.announcement_rect;
        upgrade_cta_rect = rects.upgrade_cta_rect;
        #[cfg(feature = "local-workspace")]
        {
            workspace_mode_rects = rects.workspace_mode_rects;
        }
        (rects.menu_rects, None)
    } else {
        // The home column: the cat, the wordmark under it, then the menu.
        // Inset the menu the same as the input bar (`prompt_inset`) so it keeps side spacing instead of touching the window edge on narrow terminals
        render_logo_tier(layout.logo, buf, theme, layout.logo_tier);
        render_version_badge(
            layout.wordmark,
            buf,
            theme,
            p.team_name,
            h_margin,
            p.is_api_key_auth,
            // Name and version only: the tier and the auth method are setup
            // facts, and the home is for the cat and the menu.
            VersionBadgeMode::HeroInline,
            Alignment::Left,
        );
        let menu_area = inset_horizontal(layout.menu, prompt::prompt_inset(p.compact));
        #[cfg(feature = "local-workspace")]
        let menu_area = if show_workspace_picker {
            let picker_rect = workspace_mode::picker_area(menu_area);
            workspace_mode_rects = render_workspace_mode_picker(
                picker_rect,
                buf,
                theme,
                p.workspace_mode,
                p.mouse_pos,
                p.workspace_mode_startup_locked,
                p.workspace_mode_ack_pending,
            );
            Rect {
                y: menu_area.y + workspace_mode::WORKSPACE_MODE_MENU_ROWS,
                height: menu_area
                    .height
                    .saturating_sub(workspace_mode::WORKSPACE_MODE_MENU_ROWS),
                ..menu_area
            }
        } else {
            menu_area
        };
        (
            render_menu(
                menu_area,
                buf,
                theme,
                menu_items,
                p.selected,
                p.mouse_pos,
                MENU_MIN_WIDTH,
                Alignment::Left,
            ),
            None,
        )
    };

    // Stacked info slot below the menu (narrow layout): show the announcement or the changelog (announcement takes priority), mirroring the hero box
    // Inset to match the input bar so it lines up with the menu above.
    if layout.changelog.height > 0 {
        let info_area = inset_horizontal(layout.changelog, prompt::prompt_inset(p.compact));
        // Only an announcement takes this slot now.
        if let Some(ann) = p.announcement {
            let (block, truncated, cta_rect) = render_announcement_section(
                info_area,
                buf,
                theme,
                ann,
                MENU_MIN_WIDTH,
                content_area.height,
                p.welcome_announcement_expanded,
                p.mouse_pos,
                p.upgrade_cta,
            );
            announcement_rect = block;
            announcement_truncated = truncated;
            upgrade_cta_rect = cta_rect;
        } else {
            changelog_cta_rect = render_changelog_section(
                info_area,
                buf,
                theme,
                p.changelog_bullets,
                MENU_MIN_WIDTH,
                content_area.height,
                p.changelog_has_full_notes,
                p.mouse_pos,
            );
        }
    }

    // Skip the prompt input when picker is visible to save space; shortcuts are rendered inside the picker content area
    let mut refresh_hit_rect: Option<Rect> = None;
    let mut gate_url_hit_rect: Option<Rect> = None;
    let mut privacy_banner_opt_in_rect: Option<Rect> = None;
    let mut privacy_banner_opt_out_rect: Option<Rect> = None;
    let mut privacy_banner_terms_rect: Option<Rect> = None;
    let mut privacy_banner_policy_rect: Option<Rect> = None;
    let (cursor_pos, post_flush_escapes) = if show_picker {
        (None, None)
    } else if !p.has_access {
        // Show CTA message and version instead of the prompt.
        let [_, centered, _] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(content_area.width),
            Constraint::Min(0),
        ])
        .flex(Flex::Center)
        .areas(layout.prompt);
        // Show the user's current tier and a clickable refresh button above the gate message
        let tier_label = p.subscription_tier.unwrap_or("Free");
        let tier_prefix = format!("Tier: {tier_label}  ");
        let refresh_text = "[Refresh]";
        let total_width = tier_prefix.len() + refresh_text.len();
        let tier_line = Line::from(vec![
            Span::styled("Tier: ", Style::default().fg(theme.gray)),
            Span::styled(
                tier_label,
                Style::default()
                    .fg(theme.gray_bright)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default()),
            Span::styled(
                refresh_text,
                Style::default()
                    .fg(theme.accent_user)
                    .add_modifier(Modifier::UNDERLINED),
            ),
        ])
        .alignment(Alignment::Center);
        let tier_area = Rect {
            height: 1,
            ..centered
        };
        Paragraph::new(tier_line).render(tier_area, buf);

        // Compute the click rect for "[Refresh]" within the centered line.
        let line_start_x = tier_area.x + tier_area.width.saturating_sub(total_width as u16) / 2;
        refresh_hit_rect = Some(Rect {
            x: line_start_x + tier_prefix.len() as u16,
            y: tier_area.y,
            width: refresh_text.len() as u16,
            height: 1,
        });

        let gate_text = p
            .gate
            .map(|g| g.message.as_str())
            .unwrap_or("SuperGrok subscription required");
        let msg = Line::from(Span::styled(
            gate_text,
            Style::default().fg(theme.gray_bright),
        ))
        .alignment(Alignment::Center);
        Paragraph::new(msg).render(
            Rect {
                y: centered.y + 1,
                height: 1,
                ..centered
            },
            buf,
        );

        if centered.height > 2 {
            let url_area = Rect {
                y: centered.y + 2,
                height: 1,
                ..centered
            };
            let gate_link = p
                .gate
                .and_then(|g| g.url.as_deref())
                .unwrap_or("https://grok.com/supergrok?referrer=remote-code");
            let url = Line::from(Span::styled(
                gate_link,
                Style::default()
                    .fg(theme.accent_user)
                    .add_modifier(Modifier::UNDERLINED),
            ))
            .alignment(Alignment::Center);
            Paragraph::new(url).render(url_area, buf);

            // Compute click rect for the gate URL text (centered within url_area).
            let link_width = gate_link.len() as u16;
            let link_x = url_area.x + url_area.width.saturating_sub(link_width) / 2;
            gate_url_hit_rect = Some(Rect {
                x: link_x,
                y: url_area.y,
                width: link_width.min(url_area.width),
                height: 1,
            });
        }

        // The badge lives under the cat on this screen, so the bottom row keeps
        // only what the tip slot and a pending hint need.
        (None, None)
    } else {
        // Privacy banner owns the tip slot when visible (above the prompt), except a pending-update notification, which outranks it
        if p.privacy_banner && p.pending_update_version.is_none() && layout.tip.height > 0 {
            let [_, tip_centered, _] = Layout::horizontal([
                Constraint::Min(0),
                Constraint::Length(content_area.width),
                Constraint::Min(0),
            ])
            .flex(Flex::Center)
            .areas(layout.tip);
            let inset = prompt::prompt_inset(p.compact);
            let tip_inset = Rect {
                x: tip_centered.x + inset,
                y: tip_centered.y,
                width: tip_centered.width.saturating_sub(inset * 2),
                height: tip_centered.height,
            };
            let rects = crate::views::privacy_banner::render(tip_inset, buf, theme, p.mouse_pos);
            privacy_banner_opt_in_rect = Some(rects.opt_in);
            privacy_banner_opt_out_rect = Some(rects.opt_out);
            privacy_banner_terms_rect = Some(rects.terms);
            privacy_banner_policy_rect = Some(rects.policy);
        } else if let Some(ver) = p.pending_update_version
            && layout.tip.height > 0
        {
            // Background update notification in the tip area.
            let [_, tip_centered, _] = Layout::horizontal([
                Constraint::Min(0),
                Constraint::Length(content_area.width),
                Constraint::Min(0),
            ])
            .flex(Flex::Center)
            .areas(layout.tip);
            let inset = prompt::prompt_inset(p.compact);
            let tip_inset = Rect {
                x: tip_centered.x + inset,
                y: tip_centered.y,
                width: tip_centered.width.saturating_sub(inset * 2),
                height: tip_centered.height,
            };
            let key_name = "ctrl+u";
            let line = Line::from(vec![
                Span::styled(
                    "Update: ",
                    Style::default()
                        .fg(theme.accent_user)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("v{ver} available, press {key_name} to restart"),
                    Style::default().fg(theme.accent_user),
                ),
            ]);
            Paragraph::new(line)
                .style(Style::default().bg(theme.bg_base))
                .render(tip_inset, buf);
        }

        // Recent foreign session: offer a one-click resume in the tip area (only when no update is pending; the update shares ctrl+u and wins)
        if !p.privacy_banner
            && p.pending_update_version.is_none()
            && let Some(hint) = p.foreign_resume_hint
            && layout.tip.height > 0
        {
            let [_, tip_centered, _] = Layout::horizontal([
                Constraint::Min(0),
                Constraint::Length(content_area.width),
                Constraint::Min(0),
            ])
            .flex(Flex::Center)
            .areas(layout.tip);
            let inset = prompt::prompt_inset(p.compact);
            let tip_inset = Rect {
                x: tip_centered.x + inset,
                y: tip_centered.y,
                width: tip_centered.width.saturating_sub(inset * 2),
                height: tip_centered.height,
            };
            let mins = hint.age.as_secs() / 60;
            let when = if mins == 0 {
                "moments ago".to_string()
            } else {
                format!("{mins}m ago")
            };
            let accent = Style::default().fg(theme.accent_user);
            let accent_bold = accent.add_modifier(Modifier::BOLD);
            let tool = crate::app::foreign_tool_display_label(hint.tool);
            let line = Line::from(vec![
                Span::styled("Coming from ", accent),
                Span::styled(tool, accent_bold),
                Span::styled(format!("? Resume your session from {when} using "), accent),
                Span::styled("ctrl+u", accent_bold),
            ]);
            Paragraph::new(line)
                .style(Style::default().bg(theme.bg_base))
                .render(tip_inset, buf);
        }

        let warning = p.credit_balance.and_then(|bal| {
            crate::views::credit_bar::usage_warning(bal, p.auto_topup, p.usage_visible)
        });
        let (usage_warning_text, usage_warning_critical) = match warning {
            Some((text, critical)) => (Some(text), critical),
            None => (None, false),
        };
        let usage_info = PromptInfo {
            model_name: p.model_name,
            flags: p.flags,
            multiline: false,
            usage_warning: usage_warning_text.as_deref(),
            usage_warning_critical,
        };

        render_prompt_and_version(
            &layout,
            content_area.width,
            buf,
            theme,
            p.prompt_focus,
            prompt,
            &usage_info,
            if p.privacy_banner
                || p.pending_update_version.is_some()
                || p.foreign_resume_hint.is_some()
            {
                // Banner/update/resume tip already rendered above with custom styling.
                None
            } else {
                p.tip
            },
            p.team_name,
            h_margin,
            p.compact,
            p.pending_hint,
            p.is_api_key_auth,
            layout.has_hero_box(),
        )
    };

    WelcomeRenderResult {
        cursor_pos,
        post_flush_escapes,
        menu_rects,
        prompt_rect: if show_picker || !p.has_access {
            None
        } else {
            Some(layout.prompt)
        },
        session_picker_hit_areas: picker_close_button,
        import_banner_rect,
        auth_url_rect: None,
        auth_fallback_rect: None,
        refresh_rect: refresh_hit_rect,
        gate_url_rect: gate_url_hit_rect,
        consent_link_rects: Vec::new(),
        consent_legibility: None,
        changelog_cta_rect,
        announcement_truncated,
        announcement_rect,
        upgrade_cta_rect,
        privacy_banner_opt_in_rect,
        privacy_banner_opt_out_rect,
        privacy_banner_terms_rect,
        privacy_banner_policy_rect,
        #[cfg(feature = "local-workspace")]
        workspace_mode_rects,
    }
}

pub(crate) fn render_session_picker_body(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    ctx: &mut SessionPickerRenderCtx<'_>,
) -> crate::views::picker::PickerHitAreas {
    use crate::views::picker::{self, PickerConfig, PickerEntry, PickerField, PickerRow};
    use crate::views::session_picker::{
        SessionEntryData, build_grouped_picker_entries, build_session_entry_data,
    };

    let entries_data = match ctx.sessions {
        Some(s) => s,
        None => &[],
    };

    // Filter entries by query and source (shared helper)
    // The same effective query must drive filtering AND the content header/rows gates below
    // Otherwise this render disagrees with `handle_welcome_input`'s `build_entry_map` (which receives the effective query) on row indices
    let filter_query =
        crate::views::session_picker::effective_filter_query(ctx.state.query(), ctx.entries_query);
    let filtered_indices =
        crate::app::app_view::filter_session_entries(ctx.sessions, filter_query, ctx.source_filter);

    let content_width = area.width; // approximate for truncation
    let built = build_session_entry_data(entries_data, &filtered_indices, ctx.state, content_width);

    // Build PickerEntry refs that borrow from `built`.
    let fields_vecs: Vec<Vec<PickerField>> = built
        .iter()
        .map(|b| {
            b.field_data
                .iter()
                .map(|(l, v)| PickerField { label: l, value: v })
                .collect()
        })
        .collect();

    // Build picker entries, optionally grouped by repo_name.
    let (mut picker_entries, non_selectable_indices) = if ctx.grouped {
        let current_repo =
            crate::views::session_picker::repo_name_from_cwd(&ctx.cwd.to_string_lossy());
        build_grouped_picker_entries(
            entries_data,
            &filtered_indices,
            &built,
            &fields_vecs,
            ctx.state,
            Some(current_repo.as_str()),
        )
    } else {
        let entries: Vec<PickerEntry> = built
            .iter()
            .zip(fields_vecs.iter())
            .map(|(b, fields)| {
                PickerEntry::Row(PickerRow {
                    label: &b.summary,
                    right_label: &b.right_text,
                    selected: b.is_selected,
                    expanded: b.is_expanded,
                    fields,
                    description_lines: &[],
                    summary_lines: &[],
                    dimmed: false,
                    indent: 0,
                    badge: b.badge,
                    badge_color: None,
                    collapsible: b.collapsible,
                    underline_last_desc: false,
                })
            })
            .collect();
        (entries, Vec::new())
    };

    // Append content search result rows (shared helper handles dedup).
    use crate::views::session_picker::{build_content_entry_data, build_content_header_label};
    // Content rows will start after fuzzy rows + 1 header row.
    let content_start = picker_entries.len() + 1;
    let content_entry_data: Vec<SessionEntryData> = if let Some(hits) = ctx.content_results
        && !ctx.source_filter.is_content_search_disabled()
        && !filter_query.is_empty()
    {
        build_content_entry_data(
            hits,
            entries_data,
            &filtered_indices,
            ctx.state,
            content_start,
        )
    } else {
        Vec::new()
    };

    // Show header only if there are actual deduped content rows to display.
    let has_content_rows = !content_entry_data.is_empty();
    let content_loading = ctx.content_loading && !ctx.source_filter.is_content_search_disabled();
    let spinner_label = build_content_header_label(content_loading, has_content_rows, ctx.tick);
    // Only show the header when content results exist or when content search is in progress with a
    // non-empty query. This must match the header condition inside `build_entry_map` as called from
    // `handle_welcome_input` (app_view.rs).
    let show_content_header =
        has_content_rows || (content_loading && !filter_query.trim().is_empty());
    if show_content_header {
        picker_entries.push(PickerEntry::Header {
            label: &spinner_label,
        });
    }

    let content_fields: Vec<Vec<PickerField>> = content_entry_data
        .iter()
        .map(|b| {
            b.field_data
                .iter()
                .map(|(l, v)| PickerField { label: l, value: v })
                .collect()
        })
        .collect();

    let content_snippets: Vec<[&str; 1]> = content_entry_data
        .iter()
        .map(|b| [b.snippet_preview.as_deref().unwrap_or("")])
        .collect();

    for (i, (b, fields)) in content_entry_data
        .iter()
        .zip(content_fields.iter())
        .enumerate()
    {
        let has_snippet = b.snippet_preview.is_some();
        picker_entries.push(PickerEntry::Row(PickerRow {
            label: &b.summary,
            right_label: &b.right_text,
            selected: b.is_selected,
            expanded: b.is_expanded,
            fields,
            description_lines: if has_snippet {
                content_snippets.get(i).map(|s| s.as_slice()).unwrap_or(&[])
            } else {
                &[]
            },
            summary_lines: &[],
            dimmed: false,
            indent: 1,
            badge: if has_snippet { "match" } else { "" },
            badge_color: Some(theme.accent_user),
            collapsible: true,
            underline_last_desc: false,
        }));
    }

    let controls_hidden = ctx.chat_mode;
    let hidden_hint = if controls_hidden {
        None
    } else {
        crate::views::session_picker::hidden_external_hint(ctx.sessions, ctx.source_filter)
    };

    // Build shortcuts for fullscreen mode. Chat mode drops the worktree / deep-search / filter hints (local-Build-row actions).
    let worktree_shortcut: &'static str = "ctrl+w";
    use crate::views::shortcuts_bar::HintItem;
    let mut default_shortcuts: Vec<HintItem> = vec![
        HintItem::new(crate::key!(Esc), "back"),
        HintItem::new(crate::key!(Enter), "select"),
    ];
    if !controls_hidden {
        default_shortcuts.push(HintItem {
            keys: vec![],
            label: "worktree".into(),
            custom_display: Some(worktree_shortcut),
            description: None,
            pinned: false,
        });
    }
    default_shortcuts.push(HintItem {
        keys: vec![],
        label: "navigate".into(),
        custom_display: Some("\u{2191}\u{2193}"),
        description: None,
        pinned: false,
    });
    if ctx.pending_delete {
        default_shortcuts.clear();
        default_shortcuts.push(HintItem {
            keys: vec![],
            label: "confirm delete".into(),
            custom_display: Some("y"),
            description: None,
            pinned: false,
        });
        default_shortcuts.push(HintItem {
            keys: vec![],
            label: "cancel".into(),
            custom_display: Some("n"),
            description: None,
            pinned: false,
        });
    } else if !controls_hidden {
        default_shortcuts.push(HintItem {
            keys: vec![],
            label: "filter".into(),
            custom_display: Some("f"),
            description: None,
            pinned: false,
        });
        default_shortcuts.push(HintItem {
            keys: vec![],
            label: "delete".into(),
            custom_display: Some("d"),
            description: None,
            pinned: false,
        });
    }

    let config = PickerConfig {
        title: Some("Resume session"),
        show_search_hint: true,
        expandable: true,
        esc_clears_query: true,
        shortcuts: Some(&default_shortcuts),
        pending_hint: ctx.pending_hint,
        non_selectable: &non_selectable_indices,
        non_selectable_clickable: &[],
        shortcuts_area: ctx.shortcuts_area,
        tabs: None,
        active_tab: 0,
        filter_label: (!controls_hidden).then(|| ctx.source_filter.label()),
        filter_key_hint: (!controls_hidden).then_some("f"),
        filter_active: !controls_hidden && ctx.source_filter.is_active(),
        header_note: hidden_hint.as_deref(),
        action_keys: if controls_hidden || ctx.pending_delete {
            &[]
        } else {
            &[('d', "delete")]
        },
        disable_search: false,
        compact_bottom_bar: false,
        search_only_on_slash: false,
        vim_normal_first: crate::appearance::cache::load_vim_mode(),
    };

    picker::render_picker(
        buf,
        area,
        theme,
        ctx.state,
        &picker_entries,
        &config,
        ctx.loading,
        ctx.tick,
    )
}

/// Render the auth token input box (loopback mode).
fn render_auth_input_box(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    input: &str,
    cursor_byte: usize,
) {
    let prompt_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent_user))
        .padding(Padding {
            left: 2,
            right: 1,
            top: 0,
            bottom: 0,
        });
    let inner = prompt_block.inner(area);
    prompt_block.render(area, buf);

    if inner.height > 0 && inner.width > 2 {
        let prompt = crate::glyphs::prompt_arrow();
        let prompt_width = prompt.width() as u16;
        let input_width = inner.width.saturating_sub(prompt_width);
        let (display, cursor_column) =
            masked_auth_token_view(input, cursor_byte, input_width as usize);

        let style = if input.is_empty() {
            Style::default().fg(theme.gray_dim)
        } else {
            Style::default().fg(theme.accent_user)
        };

        let line = Line::from(vec![
            Span::styled(prompt, Style::default().fg(theme.accent_user)),
            Span::styled(display, style),
        ]);
        buf.set_line(inner.x, inner.y, &line, inner.width);
        if input_width > 0 {
            let cursor_x = inner.x + prompt_width + cursor_column as u16;
            if let Some(cell) = buf.cell_mut((cursor_x, inner.y)) {
                cell.set_style(theme.block_cursor_over(theme.bg_base));
            }
        }
    }
}

/// Only one is rendered: the severity-aware pick from `startup::banner_warning`, so a
/// runtime-pushed Warning displaces an earlier Info entry.
fn render_startup_warnings(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    warnings: &[StartupWarning],
) -> Option<Rect> {
    let w = crate::startup::banner_warning(warnings)?;

    // Skip the import-claude startup warning entirely
    // The import row in the menu carries the call-to-action with the same visual weight as every other welcome menu item
    // Showing the warning text in addition to the menu row would be redundant
    if w.message.starts_with("Import Claude settings")
        || w.message.starts_with("Claude settings detected")
    {
        return None;
    }
    let color = match w.severity {
        crate::startup::WarningSeverity::Warning => theme.warning,
        crate::startup::WarningSeverity::Info => theme.gray_dim,
    };
    let style = Style::default().fg(color);

    let mut lines: Vec<Line<'_>> = w
        .message
        .lines()
        .map(|l| Line::from(Span::styled(l, style)).alignment(Alignment::Center))
        .collect();
    if let Some(ref action) = w.action {
        lines.push(Line::from(Span::styled(action.as_str(), style)).alignment(Alignment::Center));
    }

    Paragraph::new(lines).render(area, buf);
    None
}

fn auth_token_grapheme_visible(index: usize, total: usize) -> bool {
    total <= 8 || index + 4 >= total
}

struct MaskedAuthToken {
    display: String,
    cursor_byte: usize,
}

fn build_masked_auth_token(input: &str, cursor_byte: usize) -> MaskedAuthToken {
    let graphemes: Vec<(usize, &str)> = input.grapheme_indices(true).collect();
    let total = graphemes.len();
    let mut display = String::new();
    let mut mapped_cursor = None;
    for (index, (byte, grapheme)) in graphemes.into_iter().enumerate() {
        if byte == cursor_byte {
            mapped_cursor = Some(display.len());
        }
        if auth_token_grapheme_visible(index, total) {
            display.push_str(grapheme);
        } else {
            display.push('\u{2022}');
        }
    }
    MaskedAuthToken {
        cursor_byte: mapped_cursor.unwrap_or(display.len()),
        display,
    }
}

fn masked_auth_token_view(input: &str, cursor_byte: usize, width: usize) -> (String, usize) {
    if input.is_empty() {
        return ("Paste your token here...".to_string(), 0);
    }
    let masked = build_masked_auth_token(input, cursor_byte);
    let buffer =
        xai_ratatui_textarea::EditBuffer::from_parts(masked.display.as_str(), masked.cursor_byte);
    let viewport = buffer.single_line_viewport(width);
    (
        masked
            .display
            .get(viewport.visible_byte_range)
            .unwrap_or("")
            .to_owned(),
        viewport.cursor_display_column,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::app_view::SessionPickerEntry;
    use crate::views::picker::PickerState;
    use crate::views::session_picker::{build_grouped_picker_entries, build_session_entry_data};

    fn badge_text(mode: VersionBadgeMode<'_>, team: Option<&str>) -> String {
        let area = Rect::new(0, 0, 80, 1);
        let mut buf = Buffer::empty(area);
        render_version_badge(area, &mut buf, &Theme::current(), team, 0, false, mode, Alignment::Left);
        (0..area.width)
            .map(|x| buf.cell((x, 0)).map_or(" ", |c| c.symbol()).to_string())
            .collect::<String>()
            .trim()
            .to_string()
    }

    /// The badge carries the product name, the version, and the channel, and never a release label.
    /// The hero footer prints a channel only on alpha and beta builds, so on stable it must not end on a separator.
    #[test]
    fn version_badge_carries_no_release_label() {
        let full = badge_text(
            VersionBadgeMode::Full {
                subscription_tier: None,
            },
            Some("acme"),
        );
        let inline = badge_text(VersionBadgeMode::HeroInline, None);
        let footer = badge_text(VersionBadgeMode::HeroFooter, Some("acme"));

        for rendered in [&full, &inline, &footer] {
            assert!(
                !rendered.contains("Beta"),
                "badge must not label the product: {rendered:?}"
            );
        }
        assert!(full.contains("Remote-Code"), "full badge: {full:?}");
        assert!(inline.contains("Remote-Code"), "inline badge: {inline:?}");
        assert!(footer.contains("acme"), "footer keeps the team: {footer:?}");
        assert!(
            !footer.ends_with('\u{2502}'),
            "footer must not end on a separator: {footer:?}"
        );
    }

    #[test]
    fn auth_copy_feedback_covers_delivery_states() {
        let theme = Theme::current();
        for (delivery, expected) in [
            (crate::clipboard::ClipboardDelivery::Confirmed, "copied!"),
            (
                crate::clipboard::ClipboardDelivery::Unverified,
                "copy sent: verify paste",
            ),
            (crate::clipboard::ClipboardDelivery::Failed, "copy failed"),
        ] {
            let mut lines = Vec::new();
            push_auth_copy_block(&mut lines, &theme, Some(delivery));
            let Some(line) = lines.get(3) else {
                panic!("expected copy-feedback line: {lines:?}");
            };
            let feedback = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert_eq!(feedback, expected);
        }
    }

    #[test]
    fn masked_auth_token_preserves_reveal_policy() {
        assert_eq!(
            masked_auth_token_view("", 0, 24),
            ("Paste your token here...".to_string(), 0)
        );
        assert_eq!(build_masked_auth_token("12345678", 8).display, "12345678");
        assert_eq!(build_masked_auth_token("123456789", 9).display, "•••••6789");

        let input = "abcdefghMIDDLEwxyz";
        let masked = build_masked_auth_token(input, input.len()).display;
        assert!(masked.starts_with("••••"));
        assert!(masked.ends_with("wxyz"));
        assert!(!masked.contains("MIDDLE"));
        assert!(masked.contains("\u{2022}"));

        let input = "测试令牌一二三四五六七八九十";
        let masked = build_masked_auth_token(input, input.len()).display;
        assert!(masked.starts_with("••••"));
        assert!(masked.contains("\u{2022}"));
    }

    #[test]
    fn masked_auth_mapping_handles_zero_width_combining_and_zwj_middle() {
        let prefix = "abcdefgh";
        let hidden = "\u{200b}e\u{301}👩🏽\u{200d}💻MID";
        let suffix = "wxyz";
        let token = format!("{prefix}{hidden}{suffix}");
        let before = prefix.len();
        let inside = prefix.len() + "\u{200b}e\u{301}".len();
        let after = prefix.len() + hidden.len();
        let expected = format!("{}{}", "\u{2022}".repeat(14), suffix);

        let before_masked = build_masked_auth_token(&token, before);
        let inside_masked = build_masked_auth_token(&token, inside);
        let after_masked = build_masked_auth_token(&token, after);
        assert_eq!(before_masked.display, expected);
        assert_eq!(inside_masked.display, expected);
        assert_eq!(after_masked.display, expected);
        assert_eq!(before_masked.cursor_byte, "\u{2022}".len() * 8);
        assert_eq!(inside_masked.cursor_byte, "\u{2022}".len() * 10);
        assert_eq!(after_masked.cursor_byte, "\u{2022}".len() * 14);

        for width in [1, 2, 5] {
            for cursor in [before, inside, after] {
                let (view, cursor_column) = masked_auth_token_view(&token, cursor, width);
                assert!(view.width() <= width);
                assert!(cursor_column < width);
                assert!(!view.contains('\u{200b}'));
                assert!(!view.contains("e\u{301}"));
                assert!(!view.contains("👩🏽\u{200d}💻"));
                assert!(!view.contains("MID"));
            }
        }

        let wide_prefix = "中bcdefgh";
        let wide_token = format!("{wide_prefix}HIDDEN{suffix}");
        let (_, cursor_column) = masked_auth_token_view(&wide_token, wide_prefix.len(), 40);
        assert_eq!(cursor_column, wide_prefix.graphemes(true).count());
    }

    #[test]
    fn masked_auth_render_keeps_narrow_caret_visible() {
        let token = "abcdefghSECRET-MIDDLEwxyz";
        let cursor = "abcdefghSECRET".len();
        let area = Rect::new(0, 0, 9, 3);
        let theme = Theme::current();
        let mut buffer = Buffer::empty(area);
        render_auth_input_box(area, &mut buffer, &theme, token, cursor);
        assert!((0..area.width).any(|x| {
            buffer
                .cell((x, 1))
                .is_some_and(|c| c.bg == theme.text_primary)
        }));
    }

    fn make_entry(id: &str, summary: &str, repo_name: &str) -> SessionPickerEntry {
        SessionPickerEntry {
            id: id.into(),
            summary: summary.into(),
            updated_at: chrono::Utc::now(),
            created_at: chrono::Utc::now(),
            cwd: format!("/home/user/{repo_name}"),
            hostname: None,
            source: "local".into(),
            model_id: None,
            num_messages: 1,
            last_active_at: None,
            branch: None,
            repo_name: repo_name.into(),
            worktree_label: None,
            last_turn_summary: None,
            last_recap: None,
            session_kind: None,
            card_detail: None,
        }
    }

    fn render_params<'a>(
        auth_state: &'a AuthState,
        trust_state: &'a TrustState,
        session_picker: Option<&'a [SessionPickerEntry]>,
    ) -> WelcomeRenderParams<'a> {
        WelcomeRenderParams {
            prompt_focus: WelcomePromptFocus::Unfocused,
            auth_state,
            trust_state,
            consent_state: &ConsentState::Done,
            consent_hover_link: None,
            login_label: None,
            auth_code_input: "",
            auth_code_cursor_byte: 0,
            clipboard_delivery: None,
            show_raw_url: false,
            announcement: None,
            tip: None,
            model_name: "test",
            flags: &[],
            selected: None,
            team_name: None,
            has_access: true,
            has_claude_import: false,
            mouse_pos: None,
            is_zdr_blocked: false,
            session_picker,
            session_picker_loading: false,
            compact: false,
            pending_hint: None,
            startup_warnings: &[],
            pending_update_version: None,
            foreign_resume_hint: None,
            is_api_key_auth: false,
            session_picker_content_results: None,
            session_picker_content_loading: false,
            session_picker_entries_query: None,
            welcome_tick: 0,
            gate: None,
            subscription_tier: None,
            session_picker_grouped: false,
            session_picker_source_filter: crate::views::session_picker::SourceFilter::default(),
            session_picker_pending_delete: false,
            chat_mode: false,
            cwd: std::path::Path::new("/repo"),
            credit_balance: None,
            auto_topup: None,
            usage_visible: true,
            changelog_bullets: &[],
            changelog_has_full_notes: false,
            welcome_announcement_expanded: false,
            upgrade_cta: None,
            privacy_banner: false,
            #[cfg(feature = "local-workspace")]
            workspace_mode: WelcomeWorkspaceMode::Sandbox,
            #[cfg(feature = "local-workspace")]
            workspace_mode_startup_locked: false,
            #[cfg(feature = "local-workspace")]
            workspace_mode_ack_pending: false,
        }
    }

    fn render_done_text(params: &WelcomeRenderParams<'_>) -> String {
        let area = Rect::new(0, 0, 100, 40);
        let mut buf = Buffer::empty(area);
        let mut prompt = PromptWidget::new();
        let mut picker = PickerState::default();
        render_welcome(area, &mut buf, params, &mut prompt, &mut picker);
        buffer_text(&buf)
    }

    #[test]
    fn foreign_resume_tip_names_each_tool_and_age() {
        use xai_grok_foreign_sessions::ForeignSessionTool;

        let auth = AuthState::Done;
        let trust = TrustState::Done;
        for (tool, label) in [
            (ForeignSessionTool::Claude, "Claude Code"),
            (ForeignSessionTool::Codex, "Codex"),
            (ForeignSessionTool::Cursor, "Cursor"),
        ] {
            let hint = xai_grok_foreign_sessions::RecentForeignSession {
                tool,
                native_id: "native-id".into(),
                age: std::time::Duration::from_secs(125),
            };
            let mut params = render_params(&auth, &trust, None);
            params.foreign_resume_hint = Some(&hint);
            let text = render_done_text(&params);
            assert!(text.contains(&format!("Coming from {label}?")), "{text}");
            assert!(text.contains("2m ago"), "{text}");
            assert!(text.contains("ctrl+u"), "{text}");
        }
    }

    #[test]
    fn pending_update_suppresses_foreign_resume_tip() {
        let auth = AuthState::Done;
        let trust = TrustState::Done;
        let hint = xai_grok_foreign_sessions::RecentForeignSession {
            tool: xai_grok_foreign_sessions::ForeignSessionTool::Cursor,
            native_id: "native-id".into(),
            age: std::time::Duration::from_secs(30),
        };
        let mut params = render_params(&auth, &trust, None);
        params.foreign_resume_hint = Some(&hint);
        params.pending_update_version = Some("9.9.9");

        let text = render_done_text(&params);
        assert!(text.contains("v9.9.9 available"), "{text}");
        assert!(!text.contains("Coming from Cursor?"), "{text}");
    }

    fn png() -> [u8; 8] {
        [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']
    }

    fn seed_static_owner(owner_id: u64) {
        let _ = crate::terminal::overlay::static_image(&png(), 20, 10, 0, 0, owner_id)
            .unwrap()
            .commit();
    }

    fn assert_promptless_clear(result: WelcomeRenderResult, owner_id: u64) {
        let post_flush = result
            .post_flush_escapes
            .expect("promptless welcome must clear ID 1");
        assert!(post_flush.as_str().contains("a=d"));
        let before_write =
            crate::terminal::overlay::static_image(&png(), 20, 10, 0, 0, owner_id).unwrap();
        assert!(
            !before_write.as_str().contains("a=T"),
            "constructing the clear must not commit ownership"
        );
        post_flush.write_to(&mut Vec::new()).unwrap();
        let after_write =
            crate::terminal::overlay::static_image(&png(), 20, 10, 0, 0, owner_id).unwrap();
        assert!(
            after_write.as_str().contains("a=T"),
            "writing the clear must commit ownership"
        );
    }

    #[test]
    fn authenticating_welcome_returns_paired_overlay_clear() {
        let _guard = crate::terminal::image::set_protocol_for_test(
            crate::terminal::image::GraphicsProtocol::Kitty,
        );
        crate::terminal::overlay::reset_owner();
        seed_static_owner(81);
        let auth_state = AuthState::Authenticating {
            request_seq: 1,
            handle: None,
            auth_url: None,
            mode: AuthMode::Command,
        };
        let trust_state = TrustState::Done;
        let params = render_params(&auth_state, &trust_state, None);
        let area = Rect::new(0, 0, 100, 40);
        let mut buf = Buffer::empty(area);
        let mut prompt = PromptWidget::new();
        let mut picker = PickerState::default();

        let result = render_welcome(area, &mut buf, &params, &mut prompt, &mut picker);
        assert_promptless_clear(result, 81);
    }

    #[test]
    fn picker_welcome_returns_paired_overlay_clear() {
        let _guard = crate::terminal::image::set_protocol_for_test(
            crate::terminal::image::GraphicsProtocol::Kitty,
        );
        crate::terminal::overlay::reset_owner();
        seed_static_owner(82);
        let auth_state = AuthState::Done;
        let trust_state = TrustState::Done;
        let sessions = [make_entry("session-1", "summary", "repo")];
        let params = render_params(&auth_state, &trust_state, Some(&sessions));
        let area = Rect::new(0, 0, 100, 40);
        let mut buf = Buffer::empty(area);
        let mut prompt = PromptWidget::new();
        let mut picker = PickerState::default();

        let result = render_welcome(area, &mut buf, &params, &mut prompt, &mut picker);
        assert_promptless_clear(result, 82);
    }

    /// RENDER half of the header-gate invariant (the input half is
    /// `session_picker::tests::grouped_entry_map_empty_query_with_loading_has_no_header`). A
    /// render-only header row shifts arrow-key row indices.
    #[test]
    fn render_header_gate_uses_effective_query() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let theme = crate::theme::Theme::default();
        let area = Rect::new(0, 0, 80, 20);
        // Content-only hit: title shares nothing with the query "hit".
        let entries = vec![make_entry("conv-1", "Quarterly roadmap notes", "repo")];

        let render = |entries_query: Option<&str>| -> String {
            let mut buf = Buffer::empty(area);
            let mut state = PickerState::default();
            state.set_query("hit");
            render_session_picker(
                area,
                &mut buf,
                &theme,
                SessionPickerRenderMode::Fullscreen,
                &mut SessionPickerRenderCtx {
                    state: &mut state,
                    sessions: Some(&entries),
                    cwd: std::path::Path::new("/repo"),
                    loading: false,
                    pending_hint: None,
                    shortcuts_area: None,
                    content_results: None,
                    content_loading: true,
                    entries_query,
                    tick: 0,
                    grouped: false,
                    source_filter: crate::views::session_picker::SourceFilter::default(),
                    pending_delete: false,
                    chat_mode: true,
                },
            );
            (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .map(|x| {
                            buf.cell((x, y))
                                .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                        })
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let stamped = render(Some("hit"));
        assert!(
            !stamped.contains("Searching session content"),
            "stamp==live must not render the search header:\n{stamped}"
        );
        assert!(
            stamped.contains("Quarterly roadmap notes"),
            "stamped server hit must render:\n{stamped}"
        );

        let unstamped = render(None);
        assert!(
            unstamped.contains("Searching session content"),
            "in-flight search without the stamp must render the header:\n{unstamped}"
        );
    }

    #[test]
    fn headless_hidden_external_hint_pins_above_list() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let theme = crate::theme::Theme::default();
        let area = Rect::new(0, 0, 80, 24);
        let mut entries: Vec<SessionPickerEntry> = (0..20)
            .map(|i| {
                let mut entry =
                    make_entry(&format!("s{i}"), &format!("native session {i}"), "repo");
                entry.session_kind = Some("headless".into());
                entry
            })
            .collect();
        let mut foreign = make_entry("f1", "Claude work", "repo");
        foreign.source = "claude".into();
        entries.push(foreign);

        let mut buf = Buffer::empty(area);
        let mut state = PickerState::default();
        render_session_picker(
            area,
            &mut buf,
            &theme,
            SessionPickerRenderMode::Fullscreen,
            &mut SessionPickerRenderCtx {
                state: &mut state,
                sessions: Some(&entries),
                cwd: std::path::Path::new("/repo"),
                loading: false,
                pending_hint: None,
                shortcuts_area: None,
                content_results: None,
                content_loading: false,
                entries_query: None,
                tick: 0,
                grouped: false,
                source_filter: crate::views::session_picker::SourceFilter::Headless,
                pending_delete: false,
                chat_mode: false,
            },
        );
        let screen = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| {
                        buf.cell((x, y))
                            .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let hint = screen.find("external session");
        let first_row = screen.find("native session 0");
        assert!(
            hint.is_some(),
            "Headless page must show the hidden-external hint:\n{screen}"
        );
        assert!(
            first_row.is_none() || hint.unwrap() < first_row.unwrap(),
            "hint must stay pinned above the first list row:\n{screen}"
        );
    }

    #[test]
    fn grouped_entries_insert_headers() {
        let entries = vec![
            make_entry("s1", "Fix auth", "xai"),
            make_entry("s2", "Add streaming", "xai"),
            make_entry("s3", "Nuke tables", "fw-1"),
        ];
        let indices: Vec<usize> = (0..entries.len()).collect();
        let state = PickerState::default();
        let built = build_session_entry_data(&entries, &indices, &state, 80);
        let fields_vecs: Vec<Vec<crate::views::picker::PickerField>> =
            built.iter().map(|_| Vec::new()).collect();

        let (result, non_sel) =
            build_grouped_picker_entries(&entries, &indices, &built, &fields_vecs, &state, None);

        // Two headers and three rows make five entries
        assert_eq!(result.len(), 5);
        // Groups are sorted alphabetically: fw-1 before xai.
        // Header positions: 0 (fw-1), 2 (xai)
        assert_eq!(non_sel.len(), 5);
        assert_eq!(
            non_sel,
            vec![true, false, true, false, false],
            "header/row selectability: {non_sel:?}"
        );

        // Verify headers
        assert!(
            matches!(result.first(), Some(crate::views::picker::PickerEntry::Header { label }) if label == &"fw-1")
        );
        assert!(
            matches!(result.get(2), Some(crate::views::picker::PickerEntry::Header { label }) if label == &"xai")
        );
    }

    #[test]
    fn grouped_entries_pin_current_repo_first() {
        // The render path (build_grouped_picker_entries) must pin the current working directory's repo group ahead of the alphabetical rest
        // That matches build_entry_map's index ordering
        let entries = vec![
            make_entry("s1", "Fix auth", "aaa"),
            make_entry("s2", "Add streaming", "zzz"),
        ];
        let indices: Vec<usize> = (0..entries.len()).collect();
        let state = PickerState::default();
        let built = build_session_entry_data(&entries, &indices, &state, 80);
        let fields_vecs: Vec<Vec<crate::views::picker::PickerField>> =
            built.iter().map(|_| Vec::new()).collect();

        // Pin "zzz": it leads despite sorting last alphabetically.
        let (result, _) = build_grouped_picker_entries(
            &entries,
            &indices,
            &built,
            &fields_vecs,
            &state,
            Some("zzz"),
        );
        assert!(
            matches!(result.first(), Some(crate::views::picker::PickerEntry::Header { label }) if label == &"zzz"),
            "current repo group pinned first"
        );
        assert!(
            matches!(result.get(2), Some(crate::views::picker::PickerEntry::Header { label }) if label == &"aaa"),
            "remaining group follows alphabetically"
        );
    }

    #[test]
    fn grouped_entries_single_group_has_one_header() {
        let entries = vec![
            make_entry("s1", "Fix auth", "xai"),
            make_entry("s2", "Add streaming", "xai"),
        ];
        let indices: Vec<usize> = (0..entries.len()).collect();
        let state = PickerState::default();
        let built = build_session_entry_data(&entries, &indices, &state, 80);
        let fields_vecs: Vec<Vec<crate::views::picker::PickerField>> =
            built.iter().map(|_| Vec::new()).collect();

        let (result, non_sel) =
            build_grouped_picker_entries(&entries, &indices, &built, &fields_vecs, &state, None);

        assert_eq!(result.len(), 3); // one header and two rows
        assert_eq!(non_sel, vec![true, false, false]);
    }

    #[test]
    fn grouped_entries_empty_input() {
        let entries: Vec<SessionPickerEntry> = vec![];
        let indices: Vec<usize> = vec![];
        let state = PickerState::default();
        let built = build_session_entry_data(&entries, &indices, &state, 80);
        let fields_vecs: Vec<Vec<crate::views::picker::PickerField>> = vec![];

        let (result, non_sel) =
            build_grouped_picker_entries(&entries, &indices, &built, &fields_vecs, &state, None);

        assert!(result.is_empty());
        assert!(non_sel.is_empty());
    }

    #[test]
    fn grouped_entries_rows_are_indented() {
        let entries = vec![make_entry("s1", "Fix auth", "xai")];
        let indices: Vec<usize> = vec![0];
        let state = PickerState::default();
        let built = build_session_entry_data(&entries, &indices, &state, 80);
        let fields_vecs: Vec<Vec<crate::views::picker::PickerField>> =
            built.iter().map(|_| Vec::new()).collect();

        let (result, _) =
            build_grouped_picker_entries(&entries, &indices, &built, &fields_vecs, &state, None);

        // Grouped rows are indented one column under their header
        if let Some(crate::views::picker::PickerEntry::Row(row)) = result.get(1) {
            assert_eq!(row.indent, 1);
        } else {
            panic!("expected Row, got Header");
        }
    }

    fn resume_picker_config() -> crate::views::picker::PickerConfig<'static> {
        crate::views::picker::PickerConfig {
            title: Some("Resume session"),
            show_search_hint: true,
            expandable: true,
            esc_clears_query: true,
            shortcuts: None,
            pending_hint: None,
            non_selectable: &[],
            non_selectable_clickable: &[],
            shortcuts_area: None,
            tabs: None,
            active_tab: 0,
            filter_label: None,
            filter_key_hint: None,
            filter_active: false,
            header_note: None,
            action_keys: &[],
            disable_search: false,
            compact_bottom_bar: false,
            search_only_on_slash: false,
            vim_normal_first: false,
        }
    }

    #[test]
    fn e_key_expands_selected_entry_in_resume_picker() {
        use crate::views::picker::{PickerOutcome, handle_picker_input};
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

        let mut state = PickerState::default();
        let config = resume_picker_config();
        let ev = Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        let outcome = handle_picker_input(&ev, &mut state, 3, &config);
        assert!(matches!(outcome, PickerOutcome::Expand(0)));
    }

    #[test]
    fn e_key_routes_to_search_when_active() {
        use crate::views::picker::{PickerOutcome, handle_picker_input};
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

        let mut state = PickerState::input_active();
        let config = resume_picker_config();
        let ev = Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        let outcome = handle_picker_input(&ev, &mut state, 3, &config);
        assert!(matches!(outcome, PickerOutcome::QueryChanged));
        assert_eq!(state.query(), "e");
    }

    #[test]
    fn changelog_hidden_on_short_terminal() {
        let area = Rect::new(0, 0, 80, 15);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            changelog_height: 5,
            ..Default::default()
        });
        assert_eq!(layout.changelog.height, 0);
    }

    #[test]
    fn changelog_shown_on_tall_terminal() {
        let area = Rect::new(0, 0, 80, 50);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            changelog_height: 5,
            ..Default::default()
        });
        assert_eq!(layout.changelog.height, 5);
    }

    #[test]
    fn stacked_slot_sized_for_announcement_over_changelog() {
        // Narrow terminal: 80 cols is under the 90 the hero box needs
        // With both present, the stacked info slot is sized for the announcement (priority), not the changelog
        let area = Rect::new(0, 0, 80, 50);
        let a = long_ann();
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            changelog_height: 5,
            announcement: Some(&a),
            ..Default::default()
        });
        assert!(!layout.has_hero_box());
        assert_eq!(layout.changelog.height, 3);
    }

    #[test]
    fn stacked_slot_uses_announcement_when_no_changelog() {
        // Narrow terminal, announcement but no changelog: the stacked slot is still allocated for the announcement (it used to be changelog-only)
        let area = Rect::new(0, 0, 80, 50);
        let a = long_ann();
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            announcement: Some(&a),
            ..Default::default()
        });
        assert!(!layout.has_hero_box());
        assert_eq!(layout.changelog.height, 3);
    }

    #[test]
    fn changelog_hidden_when_compact() {
        let area = Rect::new(0, 0, 80, 60);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            changelog_height: 5,
            compact: true,
            prompt_compact: true,
            ..Default::default()
        });
        assert_eq!(layout.changelog.height, 0);
    }

    #[test]
    fn changelog_hidden_when_zero_requested() {
        let area = Rect::new(0, 0, 80, 60);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            ..Default::default()
        });
        assert_eq!(layout.changelog.height, 0);
    }

    #[test]
    fn changelog_boundary_exact_fit() {
        // No logo at h < 22. fixed_above = 0 + 1 + 0 + 0 = 1. fixed_below = 0 (tip) + 0 (tip_gap) + 3
        // (prompt) + 1 (ver_gap) + 1 (ver) = 5. min_without_changelog = 1 + 4 (menu) + 1 (flex) + 5 = 11.
        // changelog slot = 1 (gap) + 5 (height) = 6. Threshold = 11 + 6 = 17.
        let just_fits = Rect::new(0, 0, 80, 17);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: just_fits,
            menu_height: 4,
            changelog_height: 5,
            ..Default::default()
        });
        assert_eq!(layout.changelog.height, 5);

        let too_short = Rect::new(0, 0, 80, 16);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: too_short,
            menu_height: 4,
            changelog_height: 5,
            ..Default::default()
        });
        assert_eq!(layout.changelog.height, 0);
    }

    #[test]
    fn changelog_hidden_when_tip_steals_space() {
        // Use narrow width to avoid hero box path, keeping stacked layout.
        // With tip_height=2: fixed_below(2) = 8. min = 1 + 4 + 1 + 8 = 14.
        // Threshold = 14 + 6 = 20. At h=19 the tip pushes changelog out.
        let with_tip = Rect::new(0, 0, 60, 19);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: with_tip,
            menu_height: 4,
            tip_height: 2,
            changelog_height: 5,
            ..Default::default()
        });
        assert_eq!(layout.changelog.height, 0);

        // Same size without tip: threshold = 17 <= 19, changelog fits.
        let without_tip = Rect::new(0, 0, 60, 19);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: without_tip,
            menu_height: 4,
            changelog_height: 5,
            ..Default::default()
        });
        assert_eq!(layout.changelog.height, 5);
    }

    #[test]
    fn hero_box_active_on_wide_tall_terminal() {
        // 90 cols, 50 rows: meets the minimum for the hero box.
        let area = Rect::new(0, 0, 90, 50);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            ..Default::default()
        });
        assert!(layout.has_hero_box(), "hero box should be active at 90x50");
        assert!(layout.hero_box.width > 0);
        assert!(layout.hero_box.height > 0);
        // Logo and menu slots are zero in hero box mode (content is inside the box).
        assert_eq!(layout.logo.width, 0);
        assert_eq!(layout.menu.width, 0);
        // Sub-rects inside the hero box are valid.
        assert!(layout.hero_logo.height > 0);
        assert!(layout.hero_menu.height > 0);
        assert_eq!(layout.hero_version.height, 1);
    }

    /// Every reserved `Length` row kept its requested height; ratatui shrinks over-committed rows instead of placing them outside the area.
    fn places_every_row(layout: &WelcomeLayout, input: &WelcomeLayoutInput<'_>) -> bool {
        let prompt_height = input.prompt_height.unwrap_or(PROMPT_HEIGHT);
        let stacked_rows =
            layout.menu.height == input.menu_height && layout.error.height == input.error_height;
        (layout.has_hero_box() || stacked_rows)
            && layout.tip.height == input.tip_height
            && layout.prompt.height == prompt_height
            && layout.version.height == 1
    }

    fn assert_places_every_row(layout: &WelcomeLayout, input: &WelcomeLayoutInput<'_>) {
        assert!(
            places_every_row(layout, input),
            "a row lost height: menu {} tip {} prompt {} version {} for prompt {:?} in {:?}",
            layout.menu.height,
            layout.tip.height,
            layout.prompt.height,
            layout.version.height,
            input.prompt_height,
            input.content_area
        );
    }

    #[test]
    fn hero_box_prompt_grows_into_the_flex_gap_and_holds_the_box_still() {
        let area = Rect::new(0, 0, 90, 50);
        let input = |prompt_height| WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            prompt_height,
            ..Default::default()
        };
        let one_line = WelcomeLayout::compute(input(None));
        let five_lines = WelcomeLayout::compute(input(Some(PROMPT_HEIGHT + 4)));
        assert!(one_line.has_hero_box() && five_lines.has_hero_box());
        assert_eq!(one_line.prompt.height, PROMPT_HEIGHT);
        assert_eq!(five_lines.prompt.height, PROMPT_HEIGHT + 4);
        assert_eq!(five_lines.version, one_line.version);
        assert_eq!(five_lines.prompt.bottom(), one_line.prompt.bottom());
        assert_eq!(
            five_lines.hero_box, one_line.hero_box,
            "a taller draft eats the flex gap before moving the hero box"
        );
        assert_places_every_row(&five_lines, &input(Some(PROMPT_HEIGHT + 4)));
    }

    #[test]
    fn hero_box_moves_up_only_once_the_flex_gap_is_gone() {
        // 90x26: an 11-row box, a one-row flex gap and an 11-row prompt fit exactly (min_content_height 25)
        let area = Rect::new(0, 0, 90, 26);
        let input = |prompt_height| WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            prompt_height,
            ..Default::default()
        };
        let one_line = WelcomeLayout::compute(input(None));
        let tall = WelcomeLayout::compute(input(Some(PROMPT_HEIGHT + 8)));
        assert!(one_line.has_hero_box() && tall.has_hero_box());
        assert_eq!(one_line.hero_box.y, 3, "centered with the one-line prompt");
        assert_eq!(tall.hero_box.y, 1, "clamped to the real slack");
        assert_eq!(tall.hero_box.height, one_line.hero_box.height);
        assert_places_every_row(&tall, &input(Some(PROMPT_HEIGHT + 8)));
    }

    #[test]
    fn hero_box_yields_to_stacked_when_the_draft_needs_its_rows() {
        // 26 rows fit the 11-row box beside a one-line prompt, not beside a 13-row draft
        let area = Rect::new(0, 0, 90, 26);
        let input = |prompt_height| WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            prompt_height,
            ..Default::default()
        };
        assert!(WelcomeLayout::compute(input(None)).has_hero_box());
        let max = prompt_max_height(&input(None));
        assert_eq!(max, 13);
        let tall = WelcomeLayout::compute(input(Some(max)));
        assert!(!tall.has_hero_box());
        // Compact logo 5 + gap 1 + menu 4 + flex 1 + prompt 13 + version 2 = 26 fits exactly
        assert_eq!(tall.logo_tier, LogoTier::Compact);
        assert_places_every_row(&tall, &input(Some(max)));
    }

    /// iTerm2's default 80x25 (content 22): the compact logo stays while the column fits and steps down only when it does not.
    #[test]
    fn stacked_logo_keeps_its_tier_until_the_column_overflows() {
        let area = Rect::new(0, 0, 80, 22);
        let input = |prompt_height| WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            prompt_height,
            ..Default::default()
        };
        let one_line = WelcomeLayout::compute(input(None));
        assert_eq!(one_line.logo_tier, LogoTier::Compact);
        // Compact logo 5 + gap 1 + menu 4 + flex 1 + prompt + version 2 = 13 + prompt: fits up to a 9-row draft
        // The one-line layout has a 5-row flex gap, so the first 4 extra rows move nothing; the next two shift the column up
        for extra in 1..=6u16 {
            let layout = WelcomeLayout::compute(input(Some(PROMPT_HEIGHT + extra)));
            assert_eq!(layout.logo_tier, LogoTier::Compact, "{extra} extra rows");
            if extra <= 4 {
                assert_eq!(
                    layout.logo, one_line.logo,
                    "{extra} extra rows: the logo holds still"
                );
                assert_eq!(
                    layout.menu.y, one_line.menu.y,
                    "{extra} extra rows: the menu holds still"
                );
            } else {
                assert_eq!(
                    layout.logo.y,
                    one_line.logo.y - (extra - 4),
                    "{extra} extra rows"
                );
            }
            assert_places_every_row(&layout, &input(Some(PROMPT_HEIGHT + extra)));
        }
        let max = prompt_max_height(&input(None));
        assert_eq!(max, 11);
        let tall = WelcomeLayout::compute(input(Some(max)));
        assert_eq!(tall.logo_tier, LogoTier::Hidden);
        assert_places_every_row(&tall, &input(Some(max)));
    }

    /// 80x33: the full logo fits beside every draft up to the cap, so it never steps down.
    #[test]
    fn stacked_full_logo_survives_the_whole_draft_range_when_it_fits() {
        let area = Rect::new(0, 0, 80, 33);
        let input = |prompt_height| WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            prompt_height: Some(prompt_height),
            ..Default::default()
        };
        let max = prompt_max_height(&input(PROMPT_HEIGHT));
        for prompt_height in PROMPT_HEIGHT..=max {
            let layout = WelcomeLayout::compute(input(prompt_height));
            assert_eq!(layout.logo_tier, LogoTier::Full, "prompt {prompt_height}");
            assert_places_every_row(&layout, &input(prompt_height));
        }
    }

    /// The cap must leave room for every other stacked row: default tip, privacy banner (2), a startup warning, the import menu row.
    /// Where even the one-line box is squeezed on main (under ~14 rows), the cap must simply refuse to grow.
    #[test]
    fn stacked_prompt_grows_and_keeps_rows_inside_short_terminals() {
        for height in [12u16, 14, 16, 18, 20, 22, 25, 30] {
            for (error_height, menu_height, tip_height) in [
                (0u16, 4u16, 0u16),
                (0, 4, 1),
                (0, 4, 2),
                (2, 5, 1),
                (3, 5, 2),
            ] {
                let area = Rect::new(0, 0, 60, height);
                let case = format!(
                    "height {height} error {error_height} menu {menu_height} tip {tip_height}"
                );
                let input = |prompt_height| WelcomeLayoutInput {
                    content_area: area,
                    error_height,
                    menu_height,
                    tip_height,
                    prompt_height: Some(prompt_height),
                    ..Default::default()
                };
                let max = prompt_max_height(&input(PROMPT_HEIGHT));
                let one_line = WelcomeLayout::compute(input(PROMPT_HEIGHT));
                if places_every_row(&one_line, &input(PROMPT_HEIGHT)) {
                    let tall = WelcomeLayout::compute(input(max));
                    assert!(
                        places_every_row(&tall, &input(max)),
                        "{case}: tall draft squeezed a row"
                    );
                } else {
                    assert_eq!(max, PROMPT_HEIGHT, "{case}: no room to grow");
                }
            }
        }
    }

    /// A draft that no longer fits beside the full logo steps the reserved rows AND the painted art down together.
    #[test]
    fn stacked_logo_art_matches_the_rows_reserved_for_a_tall_draft() {
        // Full logo 7 + gap 1 + menu 4 + flex 1 + prompt + version 2 = 15 + prompt: 26 rows fit an 11-row draft under the full logo; 13 rows need the compact one
        let area = Rect::new(0, 0, 60, 26);
        let input = |prompt_height| WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            prompt_height,
            ..Default::default()
        };
        let one_line = WelcomeLayout::compute(input(None));
        assert_eq!(one_line.logo_tier, LogoTier::Full);
        assert_eq!(one_line.logo.height, logo::full_logo_line_count());

        let tall = WelcomeLayout::compute(input(Some(13)));
        assert_eq!(tall.logo_tier, LogoTier::Compact);
        assert_eq!(tall.logo.height, logo::compact_logo_line_count());
        assert_eq!(tall.logo.height, tall.logo_tier.rows());
        assert_places_every_row(&tall, &input(Some(13)));
    }

    #[test]
    fn prompt_max_height_is_half_the_area_unless_the_column_needs_the_rows() {
        let cap = |content_height, tip_height| {
            prompt_max_height(&WelcomeLayoutInput {
                content_area: Rect::new(0, 0, 60, content_height),
                menu_height: 4,
                tip_height,
                ..Default::default()
            })
        };
        assert_eq!(cap(4, 0), PROMPT_HEIGHT);
        assert_eq!(cap(50, 1), 25);
        // 18 rows with the default tip: gap 1 + menu 4 + flex 1 + tip 2 + version 2 leaves 8, not 9
        assert_eq!(cap(18, 1), 8);
        assert_eq!(cap(18, 0), 9);
    }

    /// The consent screen passes 0 prompt rows and must sit exactly where it did before the composer could grow.
    #[test]
    fn zero_row_prompt_is_not_charged_for_a_one_line_box_when_centering() {
        // 80x28, a 9-row body, a 2-row menu: full logo 7 + gap 1 + gap 1 + body 9 leaves 10 rows
        // (10 - 4 - 2) / 3 = 1 with the zero-row box; a phantom 3-row box would give (10 - 4 - 5) / 3 = 0
        let area = Rect::new(0, 0, 80, 28);
        let layout = WelcomeLayout::compute_stacked(WelcomeLayoutInput {
            content_area: area,
            error_height: 9,
            menu_height: 2,
            prompt_height: Some(0),
            ..Default::default()
        });
        assert_eq!(layout.logo_tier, LogoTier::Full);
        assert_eq!(layout.logo.y, 1);
        assert_eq!(layout.prompt.height, 0);
    }

    /// End to end: the draft measured in `render_welcome` is what sizes the box, not a hand-fed `prompt_height`.
    #[test]
    fn render_welcome_sizes_the_composer_from_the_draft() {
        let auth = AuthState::Done;
        let trust = TrustState::Done;
        let params = render_params(&auth, &trust, None);
        let area = Rect::new(0, 0, 100, 40);
        let mut picker = PickerState::default();

        let mut prompt = PromptWidget::new();
        let mut buf = Buffer::empty(area);
        let empty = render_welcome(area, &mut buf, &params, &mut prompt, &mut picker);
        assert_eq!(empty.prompt_rect.map(|r| r.height), Some(PROMPT_HEIGHT));

        prompt.set_text("a\nb\nc");
        let mut buf = Buffer::empty(area);
        let three_lines = render_welcome(area, &mut buf, &params, &mut prompt, &mut picker);
        assert_eq!(
            three_lines.prompt_rect.map(|r| r.height),
            Some(PROMPT_HEIGHT + 2)
        );
        assert_eq!(
            three_lines.prompt_rect.map(|r| r.bottom()),
            empty.prompt_rect.map(|r| r.bottom()),
            "the box grows upward from its anchored bottom"
        );
    }

    /// Logout keeps the home draft and lands on the login screen, which must show the whole draft rather than one scrolled row.
    #[test]
    fn login_screen_sizes_the_composer_from_the_kept_draft() {
        let auth = AuthState::Pending { error: None };
        let trust = TrustState::Done;
        let params = render_params(&auth, &trust, None);
        let area = Rect::new(0, 0, 100, 40);
        let mut picker = PickerState::default();
        let mut prompt = PromptWidget::new();
        prompt.set_text("alpha-one\nbravo-two\ncharlie-three");
        let mut buf = Buffer::empty(area);
        let _ = render_welcome(area, &mut buf, &params, &mut prompt, &mut picker);
        let text = buffer_text(&buf);
        for token in ["alpha-one", "bravo-two", "charlie-three"] {
            assert!(text.contains(token), "{token} missing:\n{text}");
        }
    }

    /// Rows of the painted buffer that hold braille logo art.
    fn painted_logo_rows(buf: &Buffer) -> u16 {
        let area = buf.area;
        (area.top()..area.bottom())
            .filter(|&y| {
                (area.left()..area.right()).any(|x| {
                    buf.cell((x, y))
                        .map(|c| c.symbol())
                        .unwrap_or("")
                        .chars()
                        .any(|c| ('\u{2800}'..='\u{28FF}').contains(&c))
                })
            })
            .count() as u16
    }

    /// End to end: a draft that steps the logo tier down paints the compact art, not the full art clipped into fewer rows.
    #[test]
    fn render_welcome_paints_the_logo_tier_the_draft_leaves_room_for() {
        let auth = AuthState::Done;
        let trust = TrustState::Done;
        let params = render_params(&auth, &trust, None);
        // 60 cols keeps the stacked layout; the top bar and margins take 3 rows, so 29 rows give the 26-row content area
        // whose full logo fits beside an 11-row draft but not the 13-row cap
        let area = Rect::new(0, 0, 60, 29);
        let mut picker = PickerState::default();
        let mut prompt = PromptWidget::new();

        let mut buf = Buffer::empty(area);
        let _ = render_welcome(area, &mut buf, &params, &mut prompt, &mut picker);
        assert_eq!(painted_logo_rows(&buf), logo::full_logo_line_count());

        prompt.set_text(&["line"; 30].join("\n"));
        let mut buf = Buffer::empty(area);
        let tall = render_welcome(area, &mut buf, &params, &mut prompt, &mut picker);
        assert_eq!(tall.prompt_rect.map(|r| r.height), Some(13));
        assert_eq!(painted_logo_rows(&buf), logo::compact_logo_line_count());
    }

    #[test]
    fn hero_box_inactive_on_narrow_terminal() {
        // 80 cols is below the 90-col threshold.
        let area = Rect::new(0, 0, 80, 50);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            ..Default::default()
        });
        assert!(
            !layout.has_hero_box(),
            "hero box should be inactive at 80x50"
        );
        assert_eq!(layout.hero_box.width, 0);
    }

    #[test]
    fn hero_box_boundary_at_min_width() {
        let just_below = Rect::new(0, 0, 89, 50);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: just_below,
            menu_height: 4,
            ..Default::default()
        });
        assert!(
            !layout.has_hero_box(),
            "hero box should be inactive at 89 cols"
        );

        let at_threshold = Rect::new(0, 0, 90, 50);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: at_threshold,
            menu_height: 4,
            ..Default::default()
        });
        assert!(
            layout.has_hero_box(),
            "hero box should be active at 90 cols"
        );
    }

    #[test]
    fn hero_box_inactive_when_compact() {
        // Compact mode (session picker visible) never uses the hero box.
        let area = Rect::new(0, 0, 120, 50);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            compact: true,
            prompt_compact: true,
            ..Default::default()
        });
        assert!(
            !layout.has_hero_box(),
            "hero box should be inactive in compact mode"
        );
        assert_eq!(layout.hero_box.width, 0);
    }

    #[test]
    fn hero_box_inactive_on_short_terminal() {
        // 16 rows is one short of the 17 the box needs (11 box + 1 flex gap + 5 fixed-below), so it falls back to the stacked layout
        let area = Rect::new(0, 0, 90, 16);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            ..Default::default()
        });
        assert!(
            !layout.has_hero_box(),
            "hero box should be inactive at 90x16 (needs 17 rows)"
        );
    }

    #[test]
    fn hero_box_inactive_when_warning_would_overflow() {
        // Regression: the box is forced to the full 7-row logo, so even a 3-item menu needs 11 box rows
        // A startup warning (error_height = 2) pushes the total past height 19
        // The gate must therefore fall back to the stacked layout instead of overflowing by a row
        let area = Rect::new(0, 0, 90, 19);
        let with_warning = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            error_height: 2,
            menu_height: 3,
            ..Default::default()
        });
        assert!(!with_warning.has_hero_box());
        // The same terminal fits the box once the warning is gone.
        let no_warning = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 3,
            ..Default::default()
        });
        assert!(no_warning.has_hero_box());
    }

    #[test]
    fn blocked_layout_stays_stacked_on_wide_terminal() {
        // The login / ZDR screens render through render_welcome_blocked, which only paints the stacked logo/menu rects
        // compute_stacked must never hand them a hero-box layout (which zeroes those rects)
        // That holds even on a wide, tall terminal where the normal path picks the hero box
        let area = Rect::new(0, 0, 120, 40);
        assert!(
            WelcomeLayout::compute(WelcomeLayoutInput {
                content_area: area,
                menu_height: 2,
                ..Default::default()
            })
            .has_hero_box(),
            "sanity: the normal path should pick the hero box at 120x40"
        );
        let blocked = WelcomeLayout::compute_stacked(WelcomeLayoutInput {
            content_area: area,
            menu_height: 2,
            ..Default::default()
        });
        assert!(!blocked.has_hero_box());
        assert!(
            blocked.logo.height > 0,
            "logo must be painted on the login screen"
        );
        assert!(
            blocked.menu.height > 0,
            "menu must be painted on the login screen"
        );
    }

    #[test]
    fn hero_box_does_not_overflow_with_tall_menu() {
        // A 6-item menu makes the box 2 rows taller than the default-4 box. The centering pad (derived
        // from the default box) must be clamped. Otherwise the box gets pushed down and the version row
        // clips at exactly min_content_height.
        let area = Rect::new(0, 0, 100, 19);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 6,
            ..Default::default()
        });
        assert!(
            layout.has_hero_box(),
            "hero box should be active at the boundary"
        );
        // top_pad must clamp to 0, so the box sits at the top, not pushed down.
        assert_eq!(
            layout.hero_box.y, area.y,
            "box pushed down by unclamped pad"
        );
        assert!(
            layout.version.y + layout.version.height <= area.y + area.height,
            "version row (y={}, h={}) overflows content height {}",
            layout.version.y,
            layout.version.height,
            area.height,
        );
    }

    #[test]
    fn hero_box_height_accounts_for_borders_and_padding() {
        // At h >= 26, logo07 is used (7 lines). With menu_height=3:
        // right_col = 2 + 0 + 0 + 1 + 3 = 6, inner = max(7, 6) = 7.
        // hero_box_height = 2 (borders) + 2 (v_pad) + 7 = 11.
        let area = Rect::new(0, 0, 100, 50);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 3,
            ..Default::default()
        });
        assert!(layout.has_hero_box());
        assert_eq!(layout.hero_box.height, 11);
    }

    #[test]
    fn hero_box_logo_top_aligned() {
        let area = Rect::new(0, 0, 100, 50);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 3,
            ..Default::default()
        });
        // Logo y should be at hero_box.y + 1 (border) + 1 (v_pad).
        assert_eq!(layout.hero_logo.y, layout.hero_box.y + 2);
    }

    #[test]
    fn hero_box_with_changelog() {
        // With no announcement, the changelog renders inside the box (info slot), not in a separate area below it
        let area = Rect::new(0, 0, 100, 50);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 3,
            changelog_height: 5,
            ..Default::default()
        });
        assert!(layout.has_hero_box());
        assert_eq!(layout.changelog.height, 0);
        assert_eq!(layout.hero_info.height, 5);
        // The subtitle is hidden when the info slot is shown.
        assert_eq!(layout.hero_subtitle.height, 0);
        assert!(layout.hero_info.y > layout.hero_version.y);
    }

    #[test]
    fn hero_box_with_announcement() {
        let area = Rect::new(0, 0, 100, 50);
        let a = long_ann();
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 3,
            announcement: Some(&a),
            ..Default::default()
        });
        assert!(layout.has_hero_box());
        // Collapsed: title (1) + 2 wrapped message lines.
        assert_eq!(layout.hero_info.height, 3);
        // The subtitle is hidden when the info slot is shown.
        assert_eq!(layout.hero_subtitle.height, 0);
        assert!(layout.hero_info.y > layout.hero_version.y);
        // The menu sits one blank row below the info block, with no divider line
        assert_eq!(
            layout.hero_menu.y,
            layout.hero_info.y + layout.hero_info.height + 1
        );
    }

    #[test]
    fn hero_box_announcement_takes_priority_over_changelog() {
        // When both are present, the info slot is sized for the announcement and the changelog is suppressed (never shown outside the box)
        let area = Rect::new(0, 0, 100, 50);
        let a = long_ann();
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 3,
            changelog_height: 5,
            announcement: Some(&a),
            ..Default::default()
        });
        assert!(layout.has_hero_box());
        assert_eq!(layout.hero_info.height, 3); // announcement height, not changelog (5)
        assert_eq!(layout.changelog.height, 0);
    }

    #[test]
    fn hero_box_announcement_clamped_when_tight() {
        // A real announcement can't disable the hero box: the slot is clamped to whatever still fits (the renderer trails a `…`)
        // The box stays active rather than falling back to the stacked layout
        let area = Rect::new(0, 0, 100, 17);
        let a = long_ann();
        let without = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 3,
            ..Default::default()
        });
        assert!(without.has_hero_box());
        let with_ann = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 3,
            announcement: Some(&a),
            ..Default::default()
        });
        assert!(
            with_ann.has_hero_box(),
            "announcement clamps to fit instead of disabling the box"
        );
        assert!(with_ann.hero_info.height > 0);
        let input = WelcomeLayoutInput {
            content_area: area,
            menu_height: 3,
            announcement: Some(&a),
            ..Default::default()
        };
        assert!(
            hero_box::min_content_height(&input, with_ann.hero_info.height, PROMPT_HEIGHT)
                <= area.height,
            "clamped slot must keep the box within the area"
        );
    }

    #[test]
    fn hero_box_keeps_one_bottom_pad_below_actions() {
        // With a changelog/announcement the subtitle is hidden, but there's still exactly one padding row between the actions and the bottom border
        // (menu=4 + info=3 fills the inner, so the menu reaches the pad.)
        let area = Rect::new(0, 0, 100, 50);
        let a = long_ann();
        let no_info = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            ..Default::default()
        });
        let with_info = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            announcement: Some(&a),
            ..Default::default()
        });
        assert_eq!(no_info.hero_subtitle.height, 1);
        assert_eq!(with_info.hero_subtitle.height, 0);
        let menu_bottom = with_info.hero_menu.y + with_info.hero_menu.height;
        let border_bottom = with_info.hero_box.y + with_info.hero_box.height - 1;
        assert_eq!(
            border_bottom - menu_bottom,
            1,
            "one pad row below the actions"
        );
    }

    /// Flatten a rendered buffer into one string for substring assertions.
    fn buffer_text(buf: &Buffer) -> String {
        let area = *buf.area();
        let mut out = String::new();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn extract_user_code_parses_verification_url() {
        assert_eq!(
            extract_user_code("https://accounts.x.ai/oauth2/device?user_code=ABCD-EFGH"),
            Some("ABCD-EFGH"),
        );
        // Trailing params after the code are ignored.
        assert_eq!(
            extract_user_code("https://x.ai/oauth2/device?user_code=WXYZ-1234&foo=bar"),
            Some("WXYZ-1234"),
        );
        // A param whose name merely ends in `user_code` must not be matched.
        assert_eq!(
            extract_user_code("https://x.ai/d?foo_user_code=BAD&user_code=GOOD"),
            Some("GOOD"),
        );
        // No code param, empty code, and unexpected characters all yield None.
        assert_eq!(extract_user_code("https://x.ai/oauth2/device"), None);
        assert_eq!(extract_user_code("https://x.ai/d?user_code="), None);
        assert_eq!(extract_user_code("https://x.ai/d?user_code=AB%20CD"), None);
    }

    #[test]
    fn device_auth_arm_shows_url_and_no_paste_box() {
        let area = Rect::new(0, 0, 80, 40);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        let url = "https://accounts.x.ai/oauth2/device?user_code=ABCD-EFGH";

        let (copy_rect, fallback_rect) = render_welcome_authenticating(
            area,
            &mut buf,
            &theme,
            logo_line_count(area.height),
            Some(url),
            AuthMode::Device,
            "", // auth_code_input — unused in device mode
            0,
            None,  // clipboard_delivery
            false, // show_raw_url
        );

        let text = buffer_text(&buf);
        assert!(
            text.contains("Approve in your browser"),
            "device arm must show the approval header, got:\n{text}"
        );
        // Device code shown for the browser-match check (anti-phishing).
        assert!(
            text.contains("ABCD-EFGH"),
            "device arm must show the device code, got:\n{text}"
        );
        assert!(
            text.contains("Make sure your browser shows this code"),
            "device arm must show the code caption, got:\n{text}"
        );
        // The click-to-copy line is present
        assert!(
            text.contains("to copy"),
            "device arm must show the copy-URL affordance, got:\n{text}"
        );
        // No manual-paste box in device mode
        assert!(
            !text.contains("Paste your token"),
            "device arm must NOT render the token paste box, got:\n{text}"
        );
        // Copy and fallback links are clickable
        assert!(
            copy_rect.is_some(),
            "device arm must expose a copy hit-rect"
        );
        assert!(
            fallback_rect.is_some(),
            "device arm must expose a show-full-URL hit-rect"
        );
    }

    #[test]
    fn device_auth_arm_raw_url_mode_shows_full_url() {
        let area = Rect::new(0, 0, 80, 40);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        let url = "https://accounts.x.ai/oauth2/device?user_code=WXYZ-1234";

        render_welcome_authenticating(
            area,
            &mut buf,
            &theme,
            logo_line_count(area.height),
            Some(url),
            AuthMode::Device,
            "",
            0,
            None,
            true, // show_raw_url
        );

        let text = buffer_text(&buf);
        assert!(
            text.contains("WXYZ-1234"),
            "raw URL mode must render the full URL including the user code, got:\n{text}"
        );
    }

    #[test]
    fn raw_url_mode_centers_url_that_fits_on_one_line() {
        let area = Rect::new(0, 0, 80, 40);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        let url = "https://accounts.x.ai/oauth2/device?user_code=WXYZ-1234";

        render_welcome_authenticating(
            area,
            &mut buf,
            &theme,
            logo_line_count(area.height),
            Some(url),
            AuthMode::Device,
            "",
            0,
            None,
            true, // show_raw_url
        );

        let text = buffer_text(&buf);
        let url_line = text
            .lines()
            .find(|l| l.contains("https://"))
            .expect("raw URL mode must render the URL");
        // Whole URL on one line, not wrapped.
        assert!(url_line.contains(url), "URL must be intact: {url_line:?}");
        // Centered: leading pad within 1 cell of trailing pad (integer split).
        let lead = url_line.len() - url_line.trim_start().len();
        let trail = url_line.len() - url_line.trim_end().len();
        assert!(
            lead > 0 && lead.abs_diff(trail) <= 1,
            "URL must be horizontally centered, lead={lead} trail={trail}:\n{text}"
        );
    }

    #[test]
    fn raw_url_mode_uses_full_width_for_long_urls() {
        let area = Rect::new(0, 0, 40, 40);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        // 40-col terminal; a URL longer than one row must wrap at the exact screen edge with no leading spaces so copy-paste stays intact
        let url = "https://accounts.x.ai/oauth2/device?user_code=WXYZ-1234&extra=0123456789";

        render_welcome_authenticating(
            area,
            &mut buf,
            &theme,
            logo_line_count(area.height),
            Some(url),
            AuthMode::Device,
            "",
            0,
            None,
            true, // show_raw_url
        );

        let text = buffer_text(&buf);
        let mut lines = text.lines();
        let first = lines
            .by_ref()
            .find(|l| l.contains("https://"))
            .expect("raw URL mode must render the URL");
        let second = lines.next().expect("URL must wrap to a second row");
        // The first row is flush against both edges (full width); the remainder starts at column 0 on the next row
        assert_eq!(
            first,
            url.get(..40).unwrap_or(url),
            "long URL row must span the full terminal width:\n{text}"
        );
        assert!(
            url.get(40..).is_some_and(|rest| second.starts_with(rest)),
            "wrapped remainder must start at column 0:\n{text}"
        );
    }

    #[test]
    fn command_auth_arm_shows_url_and_waiting() {
        let area = Rect::new(0, 0, 80, 40);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        let url = "https://accounts.x.ai/oauth2/authorize?client_id=grok";

        let (copy_rect, fallback_rect) = render_welcome_authenticating(
            area,
            &mut buf,
            &theme,
            logo_line_count(area.height),
            Some(url),
            AuthMode::Command,
            "", // auth_code_input — unused
            0,
            None,  // clipboard_delivery
            false, // show_raw_url
        );

        let text = buffer_text(&buf);
        assert!(
            text.contains("A browser window will open"),
            "command arm must show the auth header, got:\n{text}"
        );
        assert!(
            text.contains("Waiting for login to complete"),
            "command arm must show the waiting status, got:\n{text}"
        );
        // No device code; that's device-flow only
        assert!(
            !text.contains("Make sure your browser shows this code"),
            "command arm must NOT show the device-code caption, got:\n{text}"
        );
        // No manual-paste box in command mode
        assert!(
            !text.contains("Paste your token"),
            "command arm must NOT render the token paste box, got:\n{text}"
        );
        // Copy and fallback links are clickable
        assert!(
            copy_rect.is_some(),
            "command arm must expose a copy hit-rect"
        );
        assert!(
            fallback_rect.is_some(),
            "command arm must expose a show-full-URL hit-rect"
        );
    }

    fn long_ann() -> xai_grok_announcements::RemoteAnnouncement {
        xai_grok_announcements::RemoteAnnouncement {
            title: Some("Security policy".into()),
            message: Some(
                "Report security incidents to the security team promptly through \
the usual channels. "
                    .repeat(60),
            ),
            ..Default::default()
        }
    }

    /// A growing draft never reflows the announcement: the hero box keeps the slot the one-line layout gave it, or yields to stacked whole.
    #[test]
    fn draft_never_shrinks_the_hero_announcement() {
        let area = Rect::new(0, 0, 100, 32);
        let ann = xai_grok_announcements::RemoteAnnouncement {
            title: Some("Upgrade".into()),
            message: Some("SuperGrok Heavy is available for your team today.".into()),
            ..Default::default()
        };
        let input = |prompt_height| WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            announcement: Some(&ann),
            has_upgrade_cta: true,
            prompt_height,
            ..Default::default()
        };
        let one_line = WelcomeLayout::compute(input(None));
        assert!(one_line.has_hero_box());
        // title + one message line + the CTA's spacer and button rows
        assert_eq!(one_line.hero_info.height, 4);

        let max = prompt_max_height(&input(None));
        let mut saw_stacked = false;
        for prompt_height in PROMPT_HEIGHT..=max {
            let layout = WelcomeLayout::compute(input(Some(prompt_height)));
            if layout.has_hero_box() {
                assert_eq!(
                    layout.hero_info.height, one_line.hero_info.height,
                    "prompt {prompt_height}: the slot must not shrink under the draft"
                );
                assert_eq!(layout.hero_box.height, one_line.hero_box.height);
            } else {
                saw_stacked = true;
                // The stacked slot shows the announcement whole or not at all
                assert!(
                    layout.changelog.height == 0 || layout.changelog.height == 4,
                    "prompt {prompt_height}: stacked slot {}",
                    layout.changelog.height
                );
            }
            assert_places_every_row(&layout, &input(Some(prompt_height)));
        }
        assert!(saw_stacked, "the cap must push the box out at 100x32");
    }

    /// The stacked announcement outranks the logo and the draft: the cap reserves its rows, and the logo steps down before it would yield.
    #[test]
    fn stacked_announcement_stays_visible_for_every_draft_height() {
        let area = Rect::new(0, 0, 70, 30);
        let ann = long_ann();
        let input = |prompt_height| WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            announcement: Some(&ann),
            prompt_height,
            ..Default::default()
        };
        let one_line = WelcomeLayout::compute(input(None));
        assert!(!one_line.has_hero_box());
        let slot = one_line.changelog.height;
        assert!(slot > 0);
        assert_eq!(one_line.logo_tier, LogoTier::Full);
        let max = prompt_max_height(&input(None));
        let mut saw_logo_yield = false;
        for prompt_height in PROMPT_HEIGHT..=max {
            let layout = WelcomeLayout::compute(input(Some(prompt_height)));
            assert_eq!(
                layout.changelog.height, slot,
                "prompt {prompt_height}: the announcement must keep its rows"
            );
            saw_logo_yield |= layout.logo_tier != LogoTier::Full;
            assert_places_every_row(&layout, &input(Some(prompt_height)));
        }
        assert!(
            saw_logo_yield,
            "the cap must be high enough to push the logo out at 70x30"
        );
    }

    /// Production hides the logo on a short terminal and keeps the announcement; a one-line draft must still do that.
    #[test]
    fn short_stacked_terminal_keeps_the_announcement_and_drops_the_logo() {
        let ann = xai_grok_announcements::RemoteAnnouncement {
            title: Some("Grok 4.6 is here!".into()),
            message: Some("Select 'Grok 4.6' under /model.".into()),
            ..Default::default()
        };
        // Compact logo 5 + gap 1 + menu 4 + slot gap 1 + slot 2 + tip 2 + flex 1 + prompt 3 + version 2 = 21 > 20
        let area = Rect::new(0, 0, 80, 20);
        let input = || WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            tip_height: 1,
            announcement: Some(&ann),
            ..Default::default()
        };
        let layout = WelcomeLayout::compute(input());
        assert_eq!(layout.changelog.height, 2, "announcement stays");
        assert_eq!(layout.logo_tier, LogoTier::Hidden, "logo yields");
        assert_places_every_row(&layout, &input());
    }

    #[test]
    fn announcement_expands_for_long_message() {
        // Wide and tall enough for the hero box; the measured info slot grows when expanded
        let area = Rect::new(0, 0, 120, 60);
        let a = long_ann();
        let collapsed = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            announcement: Some(&a),
            ..Default::default()
        });
        let expanded = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            announcement: Some(&a),
            expanded: true,
            ..Default::default()
        });
        assert!(collapsed.has_hero_box() && expanded.has_hero_box());
        // Collapsed is title (1) + 2 wrapped lines; expanded shows much more.
        assert_eq!(collapsed.hero_info.height, 3);
        assert!(
            expanded.hero_info.height > collapsed.hero_info.height,
            "expanded {} should exceed collapsed {}",
            expanded.hero_info.height,
            collapsed.hero_info.height
        );
    }

    #[test]
    fn announcement_equal_for_short_message() {
        let area = Rect::new(0, 0, 120, 60);
        let a = xai_grok_announcements::RemoteAnnouncement {
            title: Some("FYI".into()),
            message: Some("All good.".into()),
            ..Default::default()
        };
        let collapsed = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            announcement: Some(&a),
            ..Default::default()
        });
        let expanded = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            announcement: Some(&a),
            expanded: true,
            ..Default::default()
        });
        // Title (1) + a single wrapped line, identical whether expanded or not.
        assert_eq!(collapsed.hero_info.height, 2);
        assert_eq!(collapsed.hero_info.height, expanded.hero_info.height);
    }

    #[test]
    fn announcement_clamped_in_short_box() {
        let tall = Rect::new(0, 0, 120, 60);
        let short = Rect::new(0, 0, 120, 30);
        let a = long_ann();
        let tall_expanded = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: tall,
            menu_height: 4,
            announcement: Some(&a),
            expanded: true,
            ..Default::default()
        });
        let short_expanded = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: short,
            menu_height: 4,
            announcement: Some(&a),
            expanded: true,
            ..Default::default()
        });
        assert!(tall_expanded.has_hero_box() && short_expanded.has_hero_box());
        // The short box clamps the expansion below the tall-box height...
        assert!(
            short_expanded.hero_info.height < tall_expanded.hero_info.height,
            "short {} should be clamped below tall {}",
            short_expanded.hero_info.height,
            tall_expanded.hero_info.height
        );
        // ...and the clamped height still keeps the hero box within the area.
        let short_input = WelcomeLayoutInput {
            content_area: short,
            menu_height: 4,
            announcement: Some(&a),
            expanded: true,
            ..Default::default()
        };
        assert!(
            hero_box::min_content_height(
                &short_input,
                short_expanded.hero_info.height,
                PROMPT_HEIGHT
            ) <= short.height
        );
    }

    #[test]
    fn no_announcement_uses_changelog_for_info_slot() {
        // Without an announcement the info slot falls back to the changelog height (0 here, so the slot is empty)
        let area = Rect::new(0, 0, 120, 60);
        let layout = WelcomeLayout::compute(WelcomeLayoutInput {
            content_area: area,
            menu_height: 4,
            ..Default::default()
        });
        assert_eq!(layout.hero_info.height, 0);
    }

    #[test]
    fn stacked_info_width_clamps_to_available() {
        // Min menu width is MENU_MIN_WIDTH, capped at the available (inset) slot.
        let unclamped = logo::logo_visual_width(50).max(30).max(MENU_MIN_WIDTH);
        assert_eq!(stacked_info_width(200, 50, MENU_MIN_WIDTH), unclamped);
        assert_eq!(stacked_info_width(40, 50, MENU_MIN_WIDTH), 40);
    }

    #[test]
    fn stacked_expanded_announcement_allocates_slot() {
        // Narrow terminal, so the stacked layout applies
        // A long expanded announcement must still get a nonzero info slot wherever the column has room
        // Regression: over-reserving once collapsed the whole slot to zero, hiding it
        let a = long_ann();
        for height in 20u16..=60 {
            let area = Rect::new(0, 0, 80, height);
            assert!(area.width < hero_box::HERO_BOX_MIN_WIDTH);
            let layout = WelcomeLayout::compute(WelcomeLayoutInput {
                content_area: area,
                menu_height: 4,
                announcement: Some(&a),
                expanded: true,
                ..Default::default()
            });
            assert!(!layout.has_hero_box());
            // logo + gap, menu, info gap, flex gap, prompt + version rows
            let budget = height.saturating_sub(
                logo_line_count(height)
                    + 1
                    + 4
                    + 1
                    + 1
                    + WelcomeLayout::fixed_below(0, PROMPT_HEIGHT),
            );
            if budget > 0 {
                assert!(
                    layout.changelog.height > 0,
                    "height {height}: stacked slot dropped to 0 with budget {budget}"
                );
            }
        }
    }
}
