//! `/gboom` easter egg: a tiny single-level raycaster shooter rendered in the terminal via the kitty graphics protocol.
//!
//! Not production code: this is for fun and is not maintained to the standards in `crates/codegen/AGENTS.md`.
//!
//! Typing `/gboom` (and nothing else) opens a modal overlay, the same overlay the imagine-video player uses.
//! It streams PNG frames, one full kitty `a=T` retransmission per frame, at the ~30 fps animation tick.
//! The simulation steps with wall-clock `dt`, so gameplay speed is independent of the achieved frame rate.
//!
//! Controls: `W`/`↑` forward, `S`/`↓` back, `A`/`D` strafe, `←`/`→` turn, click or `Space`/`Enter` fire, `Esc`/`q` quit.
//! Moving or dragging the mouse aims, inside the modal and only while Playing.
//! Movement uses the continuous held-key model in [`game`], because terminals deliver no key-release events.
//! It eases toward a steady target while a key is held, staying smooth regardless of the OS key-repeat cadence.

#![deny(clippy::indexing_slicing)]

mod assets;
mod engine;
mod game;

pub use assets::GBOOM_RED;

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};

use engine::{FireSim, FrameBuffer, Renderer};
use game::{Control, Game};

/// Maximum rendered frame width in pixels.
/// Each frame is PNG-encoded and pushed through the PTY every tick (~100 KB, ~3 MB/s at 30 fps at this cap), within what the video player streams.
const MAX_FRAME_W: usize = 480;
/// Maximum rendered frame height in pixels.
const MAX_FRAME_H: usize = 320;
/// Assumed cell size in pixels for aspect mapping (cell aspect 0.5, consistent with `terminal::image::fit_image_to_cells`).
const CELL_PX_W: usize = 8;
const CELL_PX_H: usize = 16;
/// Largest simulation step; longer gaps (lag, suspend) are clamped.
const MAX_DT: f32 = 0.1;
/// Delay before end screens accept a dismissal key.
const END_SCREEN_GRACE: f32 = 0.8;
/// Radians of yaw per terminal cell of horizontal mouse motion.
const MOUSE_AIM_SENSITIVITY: f32 = 0.06;
/// Column jumps larger than this re-seed the baseline without yawing.
const MAX_MOUSE_AIM_DX: i32 = 12;
/// Near-black outline that keeps screen text legible over the fire.
const TEXT_OUTLINE: [u8; 3] = [16, 6, 6];

/// Where the player is in the easter egg flow.
/// Time spent in the current phase is tracked by `GboomState::phase_time` (reset on every transition).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Title,
    Playing,
    Won,
    Dead,
}

/// Result of a key event while the game modal is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GboomKeyOutcome {
    /// Close the modal (caller clears the kitty placement).
    Close,
    /// Key consumed; state may have changed.
    Changed,
}

/// HUD values for the overlay chrome.
pub struct GboomHud {
    pub hp: i32,
    pub kills: u32,
    pub total: u32,
    pub playing: bool,
}

/// Modal state for the `/gboom` easter egg.
/// The agent view owns it like the video viewer, ticks it from the animation tick, and renders it in the draw path via post-flush kitty escapes.
pub struct GboomState {
    game: Game,
    renderer: Renderer,
    fire: FireSim,
    phase: Phase,
    last_tick: Instant,
    /// Monotonic simulation generation; bumps invalidate the frame cache.
    sim_gen: u64,
    fb: FrameBuffer,
    png: Vec<u8>,
    /// `(sim_gen, w, h)` of the cached PNG in `png`.
    cached: Option<(u64, usize, usize)>,
    /// Wall-clock time inside the current phase.
    phase_time: f32,
    last_mouse_col: Option<u16>,
    /// Popup cell rect `(x, y, w, h)` for mouse hit-testing; set from draw.
    mouse_region: Option<(u16, u16, u16, u16)>,
}

impl GboomState {
    pub fn new() -> Self {
        Self {
            game: Game::new(),
            renderer: Renderer::new(),
            fire: FireSim::new(),
            phase: Phase::Title,
            last_tick: Instant::now(),
            sim_gen: 0,
            fb: FrameBuffer::new(),
            png: Vec::new(),
            cached: None,
            phase_time: 0.0,
            last_mouse_col: None,
            mouse_region: None,
        }
    }

    /// On terminals that report key releases (Kitty keyboard protocol), latch keys on press/release so the player can move and turn at once.
    /// Otherwise fall back to the timer model that bridges OS key repeats.
    /// Defaults to off; the host sets it right after construction.
    pub fn set_release_aware(&mut self, release_aware: bool) {
        self.game.set_release_aware(release_aware);
    }

    pub fn set_mouse_region(&mut self, x: u16, y: u16, width: u16, height: u16) {
        self.mouse_region = Some((x, y, width, height));
    }

    pub fn clear_mouse_region(&mut self) {
        self.mouse_region = None;
        self.last_mouse_col = None;
    }

    fn in_mouse_region(&self, col: u16, row: u16) -> bool {
        let Some((x, y, w, h)) = self.mouse_region else {
            return false;
        };
        col >= x && row >= y && col < x.saturating_add(w) && row < y.saturating_add(h)
    }

    fn control_for(code: KeyCode) -> Option<Control> {
        match code {
            KeyCode::Char('w' | 'W') | KeyCode::Up => Some(Control::Forward),
            KeyCode::Char('s' | 'S') | KeyCode::Down => Some(Control::Back),
            KeyCode::Char('a' | 'A') => Some(Control::StrafeLeft),
            KeyCode::Char('d' | 'D') => Some(Control::StrafeRight),
            KeyCode::Left => Some(Control::TurnLeft),
            KeyCode::Right => Some(Control::TurnRight),
            _ => None,
        }
    }

    /// Advance the game by wall-clock time.
    /// Every tick produces a new frame (the world, fire, and bob animations are continuous), so the caller should always redraw after ticking.
    pub fn tick(&mut self) {
        let dt = self.last_tick.elapsed().as_secs_f32().min(MAX_DT);
        self.last_tick = Instant::now();
        self.phase_time += dt;

        match self.phase {
            Phase::Title | Phase::Won | Phase::Dead => self.fire.step(),
            Phase::Playing => {
                self.game.step(dt);
                if self.game.dead() {
                    self.set_phase(Phase::Dead);
                } else if self.game.won() && self.all_corpses_settled() {
                    // The corpse check makes the win screen wait for the final death animation to play out
                    self.set_phase(Phase::Won);
                }
            }
        }

        self.sim_gen += 1;
    }

    fn set_phase(&mut self, phase: Phase) {
        self.phase = phase;
        self.phase_time = 0.0;
        self.last_mouse_col = None;
    }

    fn all_corpses_settled(&self) -> bool {
        self.game
            .imps
            .iter()
            .all(|imp| matches!(imp.state, game::ImpState::Dead))
    }

    /// Handle a key press/repeat event.
    pub fn handle_key(&mut self, key: &KeyEvent) -> GboomKeyOutcome {
        // Esc / q always quit, in every phase.
        if matches!(
            key.code,
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q')
        ) {
            return GboomKeyOutcome::Close;
        }

        match self.phase {
            Phase::Title => {
                self.set_phase(Phase::Playing);
                self.last_tick = Instant::now();
                self.sim_gen += 1;
            }
            Phase::Playing => {
                // Held keys and queued shots only take effect on the next tick, which bumps `sim_gen`, so the current frame's cache stays valid
                if let Some(control) = Self::control_for(key.code) {
                    self.game.press(control);
                } else if matches!(key.code, KeyCode::Char(' ') | KeyCode::Enter) {
                    self.game.queue_fire();
                }
            }
            Phase::Won | Phase::Dead => {
                if self.phase_time > END_SCREEN_GRACE {
                    return GboomKeyOutcome::Close;
                }
            }
        }
        GboomKeyOutcome::Changed
    }

    /// Handle a key-release event (release-aware terminals only): un-latch the corresponding movement control so the player stops that motion.
    pub fn handle_release(&mut self, key: &KeyEvent) {
        if matches!(self.phase, Phase::Playing)
            && let Some(control) = Self::control_for(key.code)
        {
            self.game.release(control);
        }
    }

    /// Moving or dragging the mouse inside the region aims; a left click there fires (Playing only).
    pub fn handle_mouse(&mut self, mouse: &MouseEvent) {
        if !matches!(self.phase, Phase::Playing) {
            return;
        }
        let in_region = self.in_mouse_region(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if in_region => {
                self.game.queue_fire();
            }
            MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                if !in_region {
                    self.last_mouse_col = None;
                    return;
                }
                let col = mouse.column;
                if let Some(prev) = self.last_mouse_col {
                    let dx = col as i32 - prev as i32;
                    if dx.abs() > MAX_MOUSE_AIM_DX {
                        self.last_mouse_col = Some(col);
                        return;
                    }
                    if dx != 0 {
                        self.game.player.angle += dx as f32 * MOUSE_AIM_SENSITIVITY;
                        self.sim_gen += 1;
                    }
                }
                self.last_mouse_col = Some(col);
            }
            _ => {}
        }
    }

    /// Un-latch all movement (on focus loss), so a release dropped while unfocused can't leave the player walking forever.
    pub fn release_all(&mut self) {
        self.game.release_all();
        self.last_mouse_col = None;
    }

    /// Lets the app layer assert that backgrounded games drop their holds.
    #[cfg(any(test, feature = "test-support"))]
    pub fn any_movement_held(&self) -> bool {
        self.game.any_held()
    }

    pub fn hud(&self) -> GboomHud {
        GboomHud {
            hp: self.game.player.hp,
            kills: self.game.kills,
            total: self.game.total_imps(),
            playing: matches!(self.phase, Phase::Playing),
        }
    }

    /// Map a cell box to the internal render resolution: match the box's pixel aspect (8x16 px cells), capped to bound PNG payload and CPU.
    pub fn frame_size_for_cells(cols: u16, rows: u16) -> (usize, usize) {
        let mut w = (cols as usize * CELL_PX_W).max(64);
        let mut h = (rows as usize * CELL_PX_H).max(64);
        let scale = (MAX_FRAME_W as f32 / w as f32)
            .min(MAX_FRAME_H as f32 / h as f32)
            .min(1.0);
        w = ((w as f32 * scale) as usize).max(64);
        h = ((h as f32 * scale) as usize).max(64);
        (w, h)
    }

    /// Render the current frame at `(w, h)` and return it PNG-encoded.
    /// Cached per `(sim_gen, w, h)` so extra draws between ticks are free.
    pub fn frame_png(&mut self, w: usize, h: usize) -> Option<&[u8]> {
        if w < 8 || h < 8 {
            return None;
        }
        if self.cached == Some((self.sim_gen, w, h)) {
            return Some(&self.png);
        }

        self.fb.resize(w, h);
        match self.phase {
            Phase::Title => self.render_title_screen(),
            Phase::Playing => self.renderer.render_game(&mut self.fb, &self.game),
            Phase::Won => self.render_end_screen("VICTORY!", [255, 214, 80]),
            Phase::Dead => self.render_end_screen("YOU DIED", assets::GBOOM_RED),
        }

        self.png.clear();
        {
            use image::codecs::png::{CompressionType, FilterType, PngEncoder};
            use image::{ExtendedColorType, ImageEncoder};
            let encoder = PngEncoder::new_with_quality(
                &mut self.png,
                CompressionType::Fast,
                FilterType::Adaptive,
            );
            if encoder
                .write_image(&self.fb.pixels, w as u32, h as u32, ExtendedColorType::Rgb8)
                .is_err()
            {
                self.cached = None;
                return None;
            }
        }
        self.cached = Some((self.sim_gen, w, h));
        Some(&self.png)
    }

    fn render_title_screen(&mut self) {
        engine::clear(&mut self.fb, [7, 7, 9]);
        self.fire.draw(&mut self.fb, 0.62);

        let h = self.fb.h as i32;
        let title_scale = (self.fb.w / 36).clamp(2, 10);
        let small_scale = (title_scale / 3).max(1);
        engine::draw_text_centered_outlined(
            &mut self.fb,
            "GBOOM",
            h / 6,
            title_scale,
            assets::GBOOM_RED,
            TEXT_OUTLINE,
        );
        engine::draw_text_centered_outlined(
            &mut self.fb,
            "KNEE-DEEP IN THE TOKENS",
            h / 6 + 8 * title_scale as i32,
            small_scale,
            [212, 168, 92],
            TEXT_OUTLINE,
        );
        // Blink the prompt line.
        if ((self.phase_time * 1.6) as u32).is_multiple_of(2) {
            engine::draw_text_centered_outlined(
                &mut self.fb,
                "PRESS ANY KEY",
                h / 6 + 8 * title_scale as i32 + 10 * small_scale as i32,
                small_scale,
                [220, 210, 190],
                TEXT_OUTLINE,
            );
        }
    }

    fn render_end_screen(&mut self, text: &str, color: [u8; 3]) {
        engine::clear(&mut self.fb, [7, 7, 9]);
        self.fire.draw(&mut self.fb, 0.5);

        let h = self.fb.h as i32;
        let scale = (self.fb.w / 40).clamp(2, 8);
        engine::draw_text_centered_outlined(&mut self.fb, text, h / 5, scale, color, TEXT_OUTLINE);

        // Show the blinking dismissal hint once the grace period is over.
        if self.phase_time > END_SCREEN_GRACE && ((self.phase_time * 1.6) as u32).is_multiple_of(2)
        {
            engine::draw_text_centered_outlined(
                &mut self.fb,
                "PRESS ANY KEY",
                h / 5 + 10 * scale as i32,
                (scale / 3).max(1),
                [220, 210, 190],
                TEXT_OUTLINE,
            );
        }
    }
}

impl Default for GboomState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn starts_on_title_and_any_key_starts_game() {
        let mut state = GboomState::new();
        assert_eq!(state.phase, Phase::Title);
        assert_eq!(
            state.handle_key(&key(KeyCode::Char('w'))),
            GboomKeyOutcome::Changed
        );
        assert_eq!(state.phase, Phase::Playing);
    }

    #[test]
    fn esc_and_q_close_in_every_phase() {
        for phase in [Phase::Title, Phase::Playing, Phase::Won, Phase::Dead] {
            for code in [KeyCode::Esc, KeyCode::Char('q')] {
                let mut state = GboomState::new();
                state.phase = phase;
                assert_eq!(state.handle_key(&key(code)), GboomKeyOutcome::Close);
            }
        }
    }

    #[test]
    fn end_screens_have_dismissal_grace_period() {
        let mut state = GboomState::new();
        state.phase = Phase::Dead;
        state.phase_time = 0.1;
        // Within the grace period, non-quit keys are swallowed.
        assert_eq!(
            state.handle_key(&key(KeyCode::Char(' '))),
            GboomKeyOutcome::Changed
        );
        state.phase_time = END_SCREEN_GRACE + 0.1;
        assert_eq!(
            state.handle_key(&key(KeyCode::Char(' '))),
            GboomKeyOutcome::Close
        );
    }

    #[test]
    fn frame_png_produces_valid_png_at_requested_size() {
        let mut state = GboomState::new();
        state.tick();
        let png = state.frame_png(320, 200).expect("png frame").to_vec();
        assert_eq!(png.get(..8), Some(&b"\x89PNG\r\n\x1a\n"[..]));
        let img = image::load_from_memory(&png).expect("decodable");
        assert_eq!((img.width(), img.height()), (320, 200));
    }

    #[test]
    fn frame_png_is_cached_until_state_changes() {
        let mut state = GboomState::new();
        state.tick();
        let a = state.frame_png(160, 100).unwrap().to_vec();
        let b = state.frame_png(160, 100).unwrap().to_vec();
        assert_eq!(a, b, "same gen + dims must reuse the cached frame");
        state.tick();
        // After a tick the fire animates, so the title frame changes.
        let c = state.frame_png(160, 100).unwrap().to_vec();
        assert_ne!(a, c, "tick must invalidate the cached frame");
    }

    #[test]
    fn frame_size_mapping_respects_caps_and_aspect() {
        // A huge cell box gets capped.
        let (w, h) = GboomState::frame_size_for_cells(300, 80);
        assert!(w <= MAX_FRAME_W && h <= MAX_FRAME_H);
        // A typical popup box keeps the cell-box pixel aspect.
        let (w, h) = GboomState::frame_size_for_cells(60, 15);
        assert_eq!(
            (w as f32 / h as f32 * 10.0).round(),
            ((60.0 * CELL_PX_W as f32) / (15.0 * CELL_PX_H as f32) * 10.0).round()
        );
        // Tiny boxes stay above the floor.
        let (w, h) = GboomState::frame_size_for_cells(1, 1);
        assert!(w >= 64 && h >= 64);
    }

    #[test]
    fn playing_to_dead_transition() {
        let mut state = GboomState::new();
        state.handle_key(&key(KeyCode::Char('w'))); // leave title
        state.game.player.hp = 0;
        state.tick();
        assert_eq!(state.phase, Phase::Dead);
    }

    #[test]
    fn playing_to_won_transition_after_corpses_settle() {
        let mut state = GboomState::new();
        state.handle_key(&key(KeyCode::Char('w')));
        let total = state.game.total_imps();
        for imp in &mut state.game.imps {
            imp.state = game::ImpState::Dead;
        }
        state.game.kills = total;
        state.tick();
        assert_eq!(state.phase, Phase::Won);
    }

    fn playing_with_region() -> GboomState {
        let mut state = GboomState::new();
        state.handle_key(&key(KeyCode::Char('w')));
        state.set_mouse_region(0, 0, 200, 50);
        state
    }

    fn mouse_at(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn mouse_moved(column: u16) -> MouseEvent {
        mouse_at(MouseEventKind::Moved, column, 10)
    }

    fn mouse_click(column: u16, row: u16) -> MouseEvent {
        mouse_at(MouseEventKind::Down(MouseButton::Left), column, row)
    }

    /// Mouse motion inside the region yaws by the column delta times `MOUSE_AIM_SENSITIVITY`; the first event only seeds the baseline.
    #[test]
    fn mouse_aim_delta_matches_column_delta() {
        let mut state = playing_with_region();
        state.game.player.angle = 0.0;
        state.handle_mouse(&mouse_moved(40));
        assert_eq!(state.game.player.angle, 0.0);
        state.handle_mouse(&mouse_moved(45));
        assert_eq!(state.game.player.angle, 5.0 * MOUSE_AIM_SENSITIVITY);
        state.handle_mouse(&mouse_moved(42));
        assert!((state.game.player.angle - 2.0 * MOUSE_AIM_SENSITIVITY).abs() < 1e-5);
    }

    #[test]
    fn mouse_aim_only_while_playing() {
        for phase in [Phase::Title, Phase::Won, Phase::Dead] {
            let mut state = GboomState::new();
            state.set_mouse_region(0, 0, 200, 50);
            state.phase = phase;
            state.game.player.angle = 0.5;
            state.handle_mouse(&mouse_moved(10));
            state.handle_mouse(&mouse_moved(50));
            assert_eq!(state.game.player.angle, 0.5);
        }
    }

    /// A continuity break (`release_all`, or a column jump past `MAX_MOUSE_AIM_DX`) re-seeds the baseline without applying the gap as yaw.
    #[test]
    fn mouse_aim_baseline_reseeds_on_discontinuity() {
        let mut state = playing_with_region();
        state.game.player.angle = 0.0;
        state.handle_mouse(&mouse_moved(10));
        state.handle_mouse(&mouse_moved(12));
        let after_aim = state.game.player.angle;
        assert_eq!(after_aim, 2.0 * MOUSE_AIM_SENSITIVITY);

        state.release_all();
        state.handle_mouse(&mouse_moved(50));
        assert_eq!(state.game.player.angle, after_aim);
        state.handle_mouse(&mouse_moved(52));
        assert_eq!(
            state.game.player.angle,
            after_aim + 2.0 * MOUSE_AIM_SENSITIVITY
        );

        state.game.player.angle = 0.0;
        state.last_mouse_col = None;
        state.handle_mouse(&mouse_moved(10));
        state.handle_mouse(&mouse_moved(10 + MAX_MOUSE_AIM_DX as u16 + 1));
        assert_eq!(state.game.player.angle, 0.0);
        state.handle_mouse(&mouse_moved(10 + MAX_MOUSE_AIM_DX as u16 + 1 + 3));
        assert_eq!(state.game.player.angle, 3.0 * MOUSE_AIM_SENSITIVITY);
    }

    /// Re-entering the region does not apply the motion that happened outside it.
    #[test]
    fn out_of_region_motion_does_not_aim() {
        let mut state = playing_with_region();
        state.set_mouse_region(20, 5, 20, 10);
        state.game.player.angle = 0.0;
        state.handle_mouse(&mouse_at(MouseEventKind::Moved, 25, 10));
        state.handle_mouse(&mouse_at(MouseEventKind::Moved, 30, 10));
        let angle = state.game.player.angle;
        assert_eq!(angle, 5.0 * MOUSE_AIM_SENSITIVITY);

        state.handle_mouse(&mouse_at(MouseEventKind::Moved, 100, 10));
        assert_eq!(state.game.player.angle, angle);
        state.handle_mouse(&mouse_at(MouseEventKind::Moved, 22, 10));
        assert_eq!(state.game.player.angle, angle);
        state.handle_mouse(&mouse_at(MouseEventKind::Moved, 24, 10));
        assert_eq!(state.game.player.angle, angle + 2.0 * MOUSE_AIM_SENSITIVITY);
    }

    /// Clicking fires through the same queue as `Space`.
    #[test]
    fn click_fires_only_playing_in_region() {
        let mut state = playing_with_region();
        state.handle_mouse(&mouse_click(40, 10));
        state.tick();
        assert!(state.game.player.muzzle > 0.0);

        let mut state = GboomState::new();
        state.set_mouse_region(0, 0, 200, 50);
        state.handle_mouse(&mouse_click(40, 10));
        state.tick();
        assert_eq!(state.game.player.muzzle, 0.0);

        state.handle_key(&key(KeyCode::Char('w')));
        state.handle_mouse(&mouse_click(250, 10));
        state.tick();
        assert_eq!(state.game.player.muzzle, 0.0);

        for phase in [Phase::Won, Phase::Dead] {
            let mut state = GboomState::new();
            state.set_mouse_region(0, 0, 200, 50);
            state.phase = phase;
            state.handle_mouse(&mouse_click(40, 10));
            state.tick();
            assert_eq!(state.game.player.muzzle, 0.0);
        }
    }

    #[test]
    fn clear_mouse_region_disables_aim_and_fire() {
        let mut state = playing_with_region();
        state.game.player.angle = 0.0;
        state.handle_mouse(&mouse_moved(40));
        state.handle_mouse(&mouse_moved(45));
        let angle = state.game.player.angle;
        state.clear_mouse_region();
        state.handle_mouse(&mouse_moved(50));
        state.handle_mouse(&mouse_moved(55));
        assert_eq!(state.game.player.angle, angle);
        state.handle_mouse(&mouse_click(40, 10));
        state.tick();
        assert_eq!(state.game.player.muzzle, 0.0);
    }
}
