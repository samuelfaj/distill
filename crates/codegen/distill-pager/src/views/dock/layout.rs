//! Row layout for the dock.
//!
//! The dock is a fixed strip above the prompt, so it holds two invariants that
//! the rest of the widget relies on:
//!
//! - It never asks for more rows than [`DockCounts::max_rows`], or the 2-row
//!   floor (plus a queue-body row) when that floor is taller.
//! - Every non-empty section keeps its header row. Sections give up rows before
//!   a header does, and a section that cannot show every row spends its last one
//!   on a `show N more` line and scrolls inside the band it was granted.

use super::{DockCounts, DockData, DockItem, Section};

/// Rows the dock may take, defaulting to its resting cap so a caller that omits
/// it cannot collapse the dock to nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MaxRows(u16);

impl MaxRows {
    pub fn new(rows: u16) -> Self {
        Self(rows.max(1))
    }

    pub fn get(self) -> u16 {
        self.0
    }

    fn rows(self) -> usize {
        self.0 as usize
    }
}

impl Default for MaxRows {
    fn default() -> Self {
        Self(super::MAX_DOCK_ROWS)
    }
}

const ROW_SECTION_COUNT: usize = 4;

/// Per-section values, keyed so callers cannot index the wrong slot.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct SectionSlots<T>([T; ROW_SECTION_COUNT])
where
    T: Copy;

impl<T: Copy + Default> SectionSlots<T> {
    /// `Queued` owns no rows of its own, so it reads as the default.
    pub fn get(&self, section: Section) -> T {
        section
            .slot()
            .and_then(|slot| self.0.get(slot).copied())
            .unwrap_or_default()
    }

    pub fn set(&mut self, section: Section, value: T) {
        if let Some(slot) = section.slot()
            && let Some(cell) = self.0.get_mut(slot)
        {
            *cell = value;
        }
    }
}

/// Rows the dock paints for one set of counts, resolved once so cursor, mouse,
/// paint, and the queue body cannot drift apart.
pub struct DockLayout {
    rows: Vec<DockItem>,
    /// Rows a section can scroll through: 0 while it is collapsed.
    scrollable: SectionSlots<usize>,
    visible: SectionSlots<usize>,
    offsets: SectionSlots<usize>,
    queue_body_rows: u16,
}

impl DockLayout {
    pub fn new(counts: &DockCounts) -> Self {
        Self::with_cap(counts, paint_cap(counts))
    }

    /// Same as [`Self::new`], but never grants more rows than `cap`. Paint and hit-testing pass the
    /// height the frame actually assigned, so a squeezed dock still spends its rows on headers instead
    /// of clipping them.
    pub fn with_cap(counts: &DockCounts, cap: usize) -> Self {
        let cap = cap.min(paint_cap(counts));
        let grants = grant_rows(counts, cap);
        let height = granted_height(counts, &grants).min(cap);

        let mut layout = Self {
            rows: Vec::with_capacity(height),
            scrollable: SectionSlots::default(),
            visible: SectionSlots::default(),
            offsets: SectionSlots::default(),
            queue_body_rows: grants.queue_body,
        };
        for (slot, section) in counts.sections().iter().enumerate() {
            if section.len == 0 {
                continue;
            }
            layout.rows.push(DockItem::Header(section.section));
            if !section.expanded {
                continue;
            }
            let Some(&granted) = grants.sections.get(slot) else {
                continue;
            };
            // A 0-row grant still has to paint the header. Emitting RevealRemaining
            // on top of that would spend an unbudgeted row and let `truncate`
            // drop a later header.
            if granted == 0 {
                continue;
            }
            // `grant_rows` never leaves a lone row, so a granted section always
            // paints at least one row of its own.
            let visible = grants.visible(slot, section.len);
            let offset = counts
                .offsets
                .get(section.section)
                .min(section.len - visible);
            layout.scrollable.set(section.section, section.len);
            layout.visible.set(section.section, visible);
            layout.offsets.set(section.section, offset);
            layout
                .rows
                .extend((offset..offset + visible).map(|row| DockItem::Row(section.section, row)));
            if visible < section.len {
                layout.rows.push(DockItem::RevealRemaining(section.section));
            }
        }
        if counts.queued > 0 {
            layout.rows.push(DockItem::Header(Section::Queued));
        }
        layout.rows.truncate(height);
        layout
    }

    pub fn rows(&self) -> &[DockItem] {
        &self.rows
    }

    /// `None` means the row belongs to the embedded queue body or sits past the
    /// content.
    pub fn item_at(&self, row: u16) -> Option<DockItem> {
        self.rows.get(row as usize).copied()
    }

    /// Rows of `section` on screen, excluding its `show N more` line.
    pub fn visible_rows(&self, section: Section) -> usize {
        self.visible.get(section)
    }

    pub fn row_offset(&self, section: Section) -> usize {
        self.offsets.get(section)
    }

    /// Rows `section` holds but is not showing. Independent of the scroll: the
    /// band moves over the same set of hidden rows.
    pub fn hidden_rows(&self, section: Section) -> usize {
        self.scrollable
            .get(section)
            .saturating_sub(self.visible.get(section))
    }

    pub fn rows_below(&self, section: Section) -> usize {
        self.scrollable
            .get(section)
            .saturating_sub(self.offsets.get(section) + self.visible.get(section))
    }

    pub fn queue_body_rows(&self) -> u16 {
        self.queue_body_rows
    }

    /// Rows `section` holds in total, painted or not.
    pub fn total_rows(&self, section: Section) -> usize {
        self.scrollable.get(section)
    }
}

/// One row section's state, in [`Section::slot`] order.
#[derive(Clone, Copy)]
pub(super) struct SectionCounts {
    pub section: Section,
    pub len: usize,
    pub expanded: bool,
    /// The user asked for this section's hidden rows, so it is served first.
    pub show_all: bool,
}

impl DockCounts {
    pub(super) fn sections(&self) -> [SectionCounts; ROW_SECTION_COUNT] {
        [
            SectionCounts {
                section: Section::Workflows,
                len: self.workflows,
                expanded: self.workflows_expanded,
                show_all: self.workflows_show_all,
            },
            SectionCounts {
                section: Section::Subagents,
                len: self.subagents,
                expanded: self.subagents_expanded,
                show_all: self.subagents_show_all,
            },
            SectionCounts {
                section: Section::Tasks,
                len: self.tasks,
                expanded: self.tasks_expanded,
                show_all: self.tasks_show_all,
            },
            SectionCounts {
                section: Section::Watchers,
                len: self.watchers,
                expanded: self.watchers_expanded,
                show_all: self.watchers_show_all,
            },
        ]
    }
}

/// Rows handed out below the headers.
#[derive(Default, Clone, Copy)]
struct RowGrants {
    sections: [usize; ROW_SECTION_COUNT],
    queue_body: u16,
}

impl RowGrants {
    /// A truncated section spends one of its granted rows on the `show N more`
    /// line, so it shows one fewer than it was given.
    fn visible(&self, slot: usize, len: usize) -> usize {
        let granted = self.sections.get(slot).copied().unwrap_or(0);
        if granted >= len {
            len
        } else {
            granted.saturating_sub(1)
        }
    }
}

/// The 2-row floor outranks the resting height, but never the caller's `cap` (past `cap`, truncate can drop a later header).
/// Only sections opened with `show N more` may grow past the resting height; revealing one never grows the others.
fn grant_rows(counts: &DockCounts, cap: usize) -> RowGrants {
    let sections = counts.sections();
    let headers =
        sections.iter().filter(|section| section.len > 0).count() + usize::from(counts.queued > 0);

    let mut grants = RowGrants::default();
    let mut want = sections.map(|section| if section.expanded { section.len } else { 0 });
    let mut queue_want = if counts.queued > 0 {
        counts.queue_body_rows as usize
    } else {
        0
    };

    // Body budget is whatever remains after headers. Floors cannot exceed it.
    let mut spare = cap.saturating_sub(headers);

    // Pass 1: one content row (or the only row, which becomes `show N more`). Two fair passes prefer
    // 1+1+1 over 2+2+0, but the queue floor sits between them so a squeezed crowded dock (headers +
    // 1+1+1 + queue) keeps a body instead of spending the last spare on a second show-more row.
    for (slot, want) in want.iter_mut().enumerate() {
        if spare == 0 {
            break;
        }
        if *want > 0
            && let Some(granted) = grants.sections.get_mut(slot)
        {
            *granted += 1;
            *want -= 1;
            spare -= 1;
        }
    }

    // The queue body keeps one row no section can take: without it the Queued header would paint
    // expanded over an empty hit area. One row is also what `floor_need` budgets for it, so the second
    // floor pass below can still reach every section; the body widens only once those floors are paid.
    let queue_floor = usize::from(queue_want > 0 && spare > 0);
    grants.queue_body = queue_floor as u16;
    queue_want -= queue_floor;
    spare -= queue_floor;

    // Pass 2: the second row a truncated section spends on `show N more`.
    for (slot, want) in want.iter_mut().enumerate() {
        if spare == 0 {
            break;
        }
        if *want > 0
            && let Some(granted) = grants.sections.get_mut(slot)
        {
            *granted += 1;
            *want -= 1;
            spare -= 1;
        }
    }

    // Floors are paid: the body may now widen toward half of what is left.
    let queue_share = queue_want.min(spare / 2);
    grants.queue_body += queue_share as u16;
    queue_want -= queue_share;
    spare -= queue_share;

    let resting = super::MAX_DOCK_ROWS as usize;

    // Inside the resting budget every section shares alike: revealing a section is purely additive, so
    // it must not take rows the others already had. Floors may already have spent past the resting
    // height; do not let share grow further inside that leftover.
    let granted: usize = grants.sections.iter().sum::<usize>() + grants.queue_body as usize;
    spare = spare.min(
        cap.min(resting)
            .saturating_sub(headers)
            .saturating_sub(granted),
    );

    spare = share_rows(&mut grants, &mut want, spare, &[true; ROW_SECTION_COUNT]);

    while queue_want > 0 && spare > 0 {
        grants.queue_body += 1;
        queue_want -= 1;
        spare -= 1;
    }

    // A lone row can show a row or say how many are hidden, never both. Collapse every unsustainable 1
    // first so the returned spare can buy a real 2-row grant (paint order) instead of upgrading.
    normalize_no_lone_rows(&sections, &mut grants, &mut want, &mut spare, queue_floor);

    // Rows past what we already granted belong to the opened sections alone.
    // Measure from `used`, not from the resting height: floors may have already
    // spent the first extra rows.
    let opened = sections.map(|section| section.show_all);
    let used = headers + grants.sections.iter().sum::<usize>() + grants.queue_body as usize;
    let extra = cap.saturating_sub(used);
    spare = share_rows(&mut grants, &mut want, extra, &opened);
    // Extra share can hand a single row to an opened section that was at 0.
    normalize_no_lone_rows(&sections, &mut grants, &mut want, &mut spare, queue_floor);
    grants
}

/// A section ends up with none of its rows, at least two, or all of them.
fn normalize_no_lone_rows(
    sections: &[SectionCounts; ROW_SECTION_COUNT],
    grants: &mut RowGrants,
    want: &mut [usize; ROW_SECTION_COUNT],
    spare: &mut usize,
    queue_floor: usize,
) {
    for (slot, section) in sections.iter().enumerate() {
        if grants.sections.get(slot).copied() != Some(1) || section.len < 2 {
            continue;
        }
        if let Some(granted) = grants.sections.get_mut(slot) {
            *granted = 0;
        }
        if let Some(w) = want.get_mut(slot) {
            *w += 1;
        }
        *spare += 1;
    }

    for (slot, section) in sections.iter().enumerate() {
        if grants.sections.get(slot).copied() != Some(0)
            || !section.expanded
            || section.len < 2
            || want.get(slot).copied().unwrap_or(0) < 2
        {
            continue;
        }
        // Only what the body holds above the floor it was granted is available;
        // robbing the floor itself would leave the Queued header over dead space.
        let spare_body = usize::from(grants.queue_body).saturating_sub(queue_floor);
        if *spare >= 2 {
            *spare -= 2;
        } else if *spare == 1 && spare_body >= 1 {
            *spare -= 1;
            grants.queue_body -= 1;
        } else if *spare == 0 && spare_body >= 2 {
            grants.queue_body -= 2;
        } else {
            continue;
        }
        if let Some(granted) = grants.sections.get_mut(slot) {
            *granted += 2;
        }
        if let Some(w) = want.get_mut(slot) {
            *w -= 2;
        }
    }
}

/// Rows the dock needs before any section has to hide a row behind a summary:
/// every header, two rows per expanded section, and one for the queue body.
/// May exceed [`super::MAX_DOCK_ROWS`]; [`grant_rows`] still honors a tighter `cap`.
fn floor_need(counts: &DockCounts) -> usize {
    let sections = counts.sections();
    let headers =
        sections.iter().filter(|section| section.len > 0).count() + usize::from(counts.queued > 0);
    let floors = sections
        .iter()
        .map(|section| {
            if section.expanded {
                section.len.min(2)
            } else {
                0
            }
        })
        .sum::<usize>();
    let queue = usize::from(counts.queued > 0 && counts.queue_body_rows > 0);
    headers + floors + queue
}

/// Rows the dock asks for: its caller's ceiling, or the floors when those are taller. Asking below
/// the floors would hide whole sections behind bare headers, so the floors win here and the frame's
/// own layout does the bounding -- whatever it assigns comes back through [`DockLayout::with_cap`].
fn paint_cap(counts: &DockCounts) -> usize {
    counts.max_rows.rows().max(floor_need(counts))
}

/// Hands `spare` rows to the eligible sections one at a time, and returns what is
/// left over.
fn share_rows(
    grants: &mut RowGrants,
    want: &mut [usize; ROW_SECTION_COUNT],
    mut spare: usize,
    eligible: &[bool; ROW_SECTION_COUNT],
) -> usize {
    while spare > 0 {
        let before = spare;
        for (slot, want) in want.iter_mut().enumerate() {
            if spare == 0 {
                break;
            }
            if eligible.get(slot).copied() == Some(true)
                && *want > 0
                && let Some(granted) = grants.sections.get_mut(slot)
            {
                *granted += 1;
                *want -= 1;
                spare -= 1;
            }
        }
        if spare == before {
            break;
        }
    }
    spare
}

/// Rows the dock asks the frame for: exactly what its sections were granted.
pub fn desired_height(data: &DockData) -> u16 {
    let counts = data.counts();
    granted_height(&counts, &grant_rows(&counts, paint_cap(&counts))) as u16
}

fn granted_height(counts: &DockCounts, grants: &RowGrants) -> usize {
    let headers = counts
        .sections()
        .iter()
        .filter(|section| section.len > 0)
        .count()
        + usize::from(counts.queued > 0);
    headers + grants.sections.iter().sum::<usize>() + grants.queue_body as usize
}

/// Whether `section` still needs `show_all` to reach every row.
pub fn is_show_all_needed(counts: &DockCounts, section: Section) -> bool {
    let mut without = *counts;
    match section {
        Section::Workflows => without.workflows_show_all = false,
        Section::Subagents => without.subagents_show_all = false,
        Section::Tasks => without.tasks_show_all = false,
        Section::Watchers => without.watchers_show_all = false,
        Section::Queued => return false,
    }
    // Judged against the resting height: `show_all` is what lifts the cap, so
    // asking with it set would always answer "needed".
    without.max_rows = MaxRows::default();
    let len = section
        .slot()
        .and_then(|slot| without.sections().get(slot).map(|s| s.len))
        .unwrap_or(0);
    DockLayout::new(&without).visible_rows(section) < len
}

#[cfg(test)]
mod invariant_tests {
    use super::*;

    fn counts(
        lens: [usize; 3],
        expanded: [bool; 3],
        opened: Option<usize>,
        queued: usize,
        body: u16,
        cap: u16,
    ) -> DockCounts {
        let mut c = DockCounts {
            subagents: lens[0],
            tasks: lens[1],
            watchers: lens[2],
            queued,
            subagents_expanded: expanded[0],
            tasks_expanded: expanded[1],
            watchers_expanded: expanded[2],
            queue_body_rows: body,
            max_rows: MaxRows::new(cap),
            ..Default::default()
        };
        match opened {
            Some(0) => c.subagents_show_all = true,
            Some(1) => c.tasks_show_all = true,
            Some(2) => c.watchers_show_all = true,
            _ => {}
        }
        c
    }

    fn headers(counts: &DockCounts) -> usize {
        counts
            .sections()
            .iter()
            .filter(|section| section.len > 0)
            .count()
            + usize::from(counts.queued > 0)
    }

    /// The contract, swept rather than sampled: whatever the counts and whatever height the frame
    /// allows, the dock never paints past its cap, never drops a header, never leaves a section with a
    /// row it cannot explain, and never robs the queue body of the row `floor_need` promised it.
    #[test]
    fn the_layout_contract_holds_across_the_grid() {
        for lens in [
            [0, 0, 0],
            [1, 0, 0],
            [2, 2, 2],
            [4, 10, 2],
            [1, 2, 3],
            [6, 1, 0],
        ] {
            for expanded in [
                [true, true, true],
                [true, false, true],
                [false, true, false],
                [false, false, false],
            ] {
                for opened in [None, Some(0), Some(1), Some(2)] {
                    for queued in [0, 1, 3] {
                        for body in [0u16, 1, 2, 5] {
                            for cap in 1u16..=16 {
                                let c = counts(lens, expanded, opened, queued, body, cap);
                                let layout = DockLayout::new(&c);
                                let rows = layout.rows();
                                let asked = paint_cap(&c);
                                let case = format!(
                                    "lens={lens:?} expanded={expanded:?} opened={opened:?} queued={queued} body={body} cap={cap}"
                                );

                                // The dock asks for its floors even past the
                                // caller's cap; what it paints is what the frame
                                // then assigns, which `with_cap` bounds.
                                assert!(
                                    rows.len() + layout.queue_body_rows() as usize <= asked,
                                    "asked past its own floors: {case} -> {rows:?} + body {}",
                                    layout.queue_body_rows()
                                );
                                let assigned = DockLayout::with_cap(&c, cap as usize);
                                assert!(
                                    assigned.rows().len() + assigned.queue_body_rows() as usize
                                        <= cap as usize,
                                    "painted past the rows the frame assigned: {case}"
                                );

                                // Nothing is hidden without an affordance saying so.
                                for section in c.sections().iter() {
                                    if !section.expanded || section.len == 0 {
                                        continue;
                                    }
                                    let visible = layout.visible_rows(section.section);
                                    assert!(
                                        visible >= 1,
                                        "{:?} asked for nothing while holding {} rows: {case}",
                                        section.section,
                                        section.len
                                    );
                                }

                                for (slot, section) in c.sections().iter().enumerate() {
                                    let header = DockItem::Header(section.section);
                                    // Headers outrank rows, so they survive right up
                                    // to a cap that can hold them all.
                                    if section.len > 0 && cap as usize >= headers(&c) {
                                        assert!(rows.contains(&header), "dropped a header: {case}");
                                    }
                                    let visible = layout.visible_rows(section.section);
                                    let reveal =
                                        rows.contains(&DockItem::RevealRemaining(section.section));
                                    if visible < section.len && section.expanded {
                                        assert!(
                                            reveal,
                                            "section {slot} hides rows with nothing saying so: {case}"
                                        );
                                    }
                                    if reveal {
                                        assert!(
                                            visible > 0,
                                            "section {slot} summarizes with no row of its own: {case}"
                                        );
                                    }
                                }

                                if queued > 0 && body > 0 && cap as usize >= floor_need(&c) {
                                    assert!(
                                        layout.queue_body_rows() >= 1,
                                        "queue body robbed below its floor: {case}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
