//! Optional, conservative local echo. Authoritative terminal cells are never mutated.
//!
//! Echo is learned from the server, with reuse for an exact learned agent prompt.
//! This reduces the chance of
//! displaying input at a non-echoing prompt, but is not an assertion about application
//! echo permissions: the endpoint protocol does not expose those permissions.

use super::prediction_profiles::{EditorProfile, ProfileUpdate, Profiles};
use super::prediction_words::WordRules;
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

use crate::protocol::{
    CellData, ClientKeyCode, ClientKeyKind, ClientMouseButton, ClientMouseKind,
    ClientMousePosition, ClientPaneInputEvent, FrameData, PaneSurfaceFrame, SurfaceRect,
};

const PREDICTION_LIFETIME: Duration = Duration::from_millis(750);
const MAX_PENDING_EDITS: usize = 256;
const LATE_ECHO_LIFETIME: Duration = Duration::from_secs(3);
const MULTI_CLICK_WINDOW: Duration = Duration::from_millis(500);

#[derive(Default)]
pub(super) struct InputPrediction {
    line: Option<PredictedLine>,
    warm_prompt: Option<WarmPrompt>,
    agent_context: Option<crate::detect::Agent>,
    machine_keys: HashMap<crate::client::endpoint::ClientEndpointId, String>,
    machine_context: Option<String>,
    profiles: Profiles,
    profile_updates: Vec<ProfileUpdate>,
    profile_resume_epoch: Option<u64>,
}

struct WarmPrompt {
    boot_id: String,
    pane_id: String,
    geometry: SurfaceRect,
    surface_size: (u16, u16),
    terminal_modes: (bool, bool),
    cursor_visible: bool,
    x: u16,
    y: u16,
    row: Vec<CellData>,
    word_rules: [WordRules; 2],
    click_trained: bool,
}

struct PredictedLine {
    boot_id: String,
    pane_id: String,
    geometry: SurfaceRect,
    surface_size: (u16, u16),
    terminal_modes: (bool, bool),
    cursor_visible: bool,
    y: u16,
    x: u16,
    input_start: u16,
    input_end: u16,
    software_style: Option<(CellData, CellData)>,
    row: Vec<CellData>,
    initial_row: Vec<CellData>,
    initial_empty: bool,
    profile_published: bool,
    profile_epoch: u64,
    placeholder: Option<(usize, CellData)>,
    projection: EditProjection,
    pending: VecDeque<PendingEdit>,
    trained: bool,
    expired: bool,
    word_rules: [WordRules; 2],
    word_learning: Option<WordLearning>,
    click_trained: bool,
    last_click: Option<Instant>,
    mouse_down: bool,
}

struct WordLearning {
    gesture: WordGesture,
    before: String,
    starts: Vec<usize>,
    sent_at: Instant,
}

#[derive(Clone, Copy)]
enum WordGesture {
    ControlW,
    AltBackspace,
}

impl WordGesture {
    fn index(self) -> usize {
        match self {
            Self::ControlW => 0,
            Self::AltBackspace => 1,
        }
    }
}

#[derive(Clone, Copy)]
enum Edit {
    Insert(char),
    Backspace,
    Left,
    Right,
    End,
    Delete,
    ClickPosition(usize),
    WordBackspace(usize),
}

struct PendingEdit {
    edit: Edit,
    sent_at: Instant,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum CellEdit {
    #[default]
    Unchanged,
    Insert,
    Erase,
    Caret,
}

#[derive(Default)]
struct EditProjection {
    cells: Vec<CellData>,
    changes: Vec<CellEdit>,
    x: usize,
    input_start: usize,
    input_end: usize,
    padding: Option<CellData>,
    caret: Option<CellData>,
}

impl EditProjection {
    fn reset(
        &mut self,
        row: &[CellData],
        x: usize,
        input_start: usize,
        input_end: usize,
        cursor: Option<SoftwareCursor<'_>>,
    ) {
        if self.cells.len() == row.len() {
            self.cells.clone_from_slice(row);
        } else {
            self.cells = row.to_vec();
        }
        self.changes.resize(row.len(), CellEdit::Unchanged);
        self.changes.fill(CellEdit::Unchanged);
        self.x = x;
        self.input_start = input_start;
        self.input_end = input_end;
        self.padding = cursor
            .as_ref()
            .map(|cursor| cursor.padding.clone())
            .or_else(|| row.get(input_end).cloned());
        self.caret = cursor.map(|cursor| cursor.cell.clone());
        self.remove_caret();
    }

    fn remove_caret(&mut self) {
        if self.caret.is_some() {
            if let Some(padding) = &self.padding {
                let cell = &mut self.cells[self.x];
                cell.fg = padding.fg;
                cell.bg = padding.bg;
                cell.modifier = padding.modifier;
            }
        }
    }

    fn push(&mut self, edit: Edit) -> bool {
        let Some(padding) = self.padding.as_ref().filter(|cell| blank_cell(cell)) else {
            return false;
        };
        let padding = padding.clone();
        self.remove_caret();
        if self.caret.is_some() {
            self.changes[self.x] = CellEdit::Erase;
        }
        match edit {
            Edit::Insert(character) => {
                // Keep one spare cell so a prediction cannot trigger wrapping.
                if self.input_end + 1 >= self.cells.len()
                    || !blank_cell(&self.cells[self.input_end])
                    || (self.caret.is_some() && self.cells[self.input_end + 1] != padding)
                    || !self.safe_segment()
                    || !self.insert_has_boundaries(character)
                {
                    return false;
                }
                for index in (self.x..self.input_end).rev() {
                    self.cells[index + 1] = self.cells[index].clone();
                    self.changes[index + 1] = CellEdit::Insert;
                }
                self.cells[self.x].clone_from(&padding);
                self.cells[self.x].symbol = character.to_string();
                self.changes[self.x] = CellEdit::Insert;
                self.x += 1;
                self.input_end += 1;
            }
            Edit::Backspace => {
                if self.x <= self.input_start || !self.safe_segment() {
                    return false;
                }
                self.x -= 1;
                for index in self.x..self.input_end - 1 {
                    self.cells[index] = self.cells[index + 1].clone();
                    self.changes[index] = CellEdit::Insert;
                }
                self.input_end -= 1;
                self.cells[self.input_end].clone_from(&padding);
                self.changes[self.input_end] = CellEdit::Erase;
            }
            Edit::Delete => {
                if self.x >= self.input_end || !self.safe_segment() {
                    return false;
                }
                for index in self.x..self.input_end - 1 {
                    self.cells[index] = self.cells[index + 1].clone();
                    self.changes[index] = CellEdit::Insert;
                }
                self.input_end -= 1;
                self.cells[self.input_end].clone_from(&padding);
                self.changes[self.input_end] = CellEdit::Erase;
            }
            Edit::Left => {
                if self.x <= self.input_start || !self.safe_segment() {
                    return false;
                }
                self.x -= 1;
            }
            Edit::Right => {
                if self.x >= self.input_end || !self.safe_segment() {
                    return false;
                }
                self.x += 1;
            }
            Edit::End => {
                self.x = self.input_end;
            }
            Edit::ClickPosition(x) => {
                if x < self.input_start || x > self.input_end || !self.safe_segment() {
                    return false;
                }
                self.x = x;
            }
            Edit::WordBackspace(count) => {
                for _ in 0..count {
                    if !self.push(Edit::Backspace) {
                        return false;
                    }
                }
            }
        }
        if let Some(caret) = &self.caret {
            let cell = &mut self.cells[self.x];
            cell.fg = caret.fg;
            cell.bg = caret.bg;
            cell.modifier = caret.modifier;
            self.changes[self.x] = CellEdit::Caret;
        }
        true
    }

    fn safe_segment(&self) -> bool {
        self.cells[self.input_start..self.input_end]
            .iter()
            .all(single_cell_symbol)
    }

    fn insert_has_boundaries(&self, character: char) -> bool {
        let mut adjacent = String::new();
        if self.x > self.input_start {
            adjacent.push_str(&self.cells[self.x - 1].symbol);
        }
        adjacent.push(character);
        if self.x < self.input_end {
            adjacent.push_str(&self.cells[self.x].symbol);
        }
        adjacent.graphemes(true).count() == adjacent.chars().count()
    }

    fn prefix(&self) -> String {
        self.cells[self.input_start..self.x]
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect()
    }

    fn matches(&self, actual: &[CellData]) -> bool {
        actual.len() == self.cells.len()
            && actual.iter().zip(&self.cells).zip(&self.changes).all(
                |((actual, expected), change)| match change {
                    CellEdit::Unchanged | CellEdit::Caret => actual == expected,
                    CellEdit::Insert | CellEdit::Erase => {
                        actual.symbol == expected.symbol
                            && !actual.skip
                            && actual.hyperlink.is_none()
                            && self.caret.as_ref().is_none_or(|_| {
                                self.padding
                                    .as_ref()
                                    .is_some_and(|padding| same_style(actual, padding))
                            })
                    }
                },
            )
    }
}

struct EligibleRow<'a> {
    geometry: SurfaceRect,
    terminal_modes: (bool, bool),
    cursor_visible: bool,
    x: u16,
    y: u16,
    cells: &'a [CellData],
}

struct SoftwareCursor<'a> {
    cell: &'a CellData,
    padding: &'a CellData,
}

fn blank_cell(cell: &CellData) -> bool {
    cell.symbol == " " && !cell.skip && cell.hyperlink.is_none()
}

fn safe_character(character: char) -> bool {
    !character.is_control()
        && UnicodeWidthChar::width(character) == Some(1)
        && crate::ghostty::unicode_codepoint_width(character as u32) == 1
}

fn single_cell_symbol(cell: &CellData) -> bool {
    let mut chars = cell.symbol.chars();
    !cell.skip
        && cell.hyperlink.is_none()
        && chars.next().is_some_and(safe_character)
        && chars.next().is_none()
}

fn agent_prompt_start(row: &[CellData], agent: crate::detect::Agent) -> Option<usize> {
    let marker = match agent {
        crate::detect::Agent::Claude => "❯",
        crate::detect::Agent::Codex => "›",
        _ => return None,
    };
    let index = row.iter().position(|cell| cell.symbol != " ")?;
    let prompt = &row[index];
    let spacing = row.get(index + 1)?;
    (prompt.symbol == marker
        && !prompt.skip
        && prompt.hyperlink.is_none()
        && matches!(spacing.symbol.as_str(), " " | "\u{a0}")
        && !spacing.skip
        && spacing.hyperlink.is_none())
    .then_some(index + 2)
    .filter(|start| *start < row.len())
}

fn prompt_fingerprint(
    row: &[CellData],
    start: u16,
    visible: bool,
    modes: (bool, bool),
) -> Option<[u8; 32]> {
    serde_json::to_vec(&(row, start, visible, modes))
        .ok()
        .map(|bytes| Sha256::digest(bytes).into())
}

fn same_style(left: &CellData, right: &CellData) -> bool {
    left.fg == right.fg && left.bg == right.bg && left.modifier == right.modifier
}

fn eligible_row<'a>(surface: &'a PaneSurfaceFrame, pane_id: &str) -> Option<EligibleRow<'a>> {
    if surface.popup.is_some() {
        return None;
    }
    let pane = surface.panes.iter().find(|pane| pane.pane_id == pane_id)?;
    if !pane.focused
        || pane
            .scroll
            .is_some_and(|scroll| scroll.offset_from_bottom != 0)
    {
        return None;
    }
    // A TUI may draw its own caret while keeping the terminal cursor hidden.
    // Coordinates alone do not grant confidence: an exact echo must still be learned.
    let cursor = surface.frame.cursor.as_ref()?;
    let geometry = pane.inner_rect;
    let right = geometry.x.checked_add(geometry.width)?;
    let bottom = geometry.y.checked_add(geometry.height)?;
    if cursor.x < geometry.x
        || cursor.x >= right
        || cursor.y < geometry.y
        || cursor.y >= bottom
        || right > surface.frame.width
        || bottom > surface.frame.height
    {
        return None;
    }
    let start = usize::from(cursor.y) * usize::from(surface.frame.width) + usize::from(geometry.x);
    Some(EligibleRow {
        geometry,
        terminal_modes: (pane.alternate_screen_active, pane.mouse_reporting),
        cursor_visible: cursor.visible,
        x: cursor.x,
        y: cursor.y,
        cells: surface
            .frame
            .cells
            .get(start..start + usize::from(geometry.width))?,
    })
}

impl PredictedLine {
    fn reset_projection(&mut self) {
        let cursor = self.software_style.clone();
        self.projection.reset(
            &self.row,
            usize::from(self.x - self.geometry.x),
            usize::from(self.input_start - self.geometry.x),
            usize::from(self.input_end - self.geometry.x),
            cursor
                .as_ref()
                .map(|(cell, padding)| SoftwareCursor { cell, padding }),
        );
        if let Some((end, padding)) = &self.placeholder {
            self.projection.padding = Some(padding.clone());
            for index in self.projection.input_start..*end {
                self.projection.cells[index].clone_from(padding);
                self.projection.changes[index] = CellEdit::Erase;
            }
        }
    }

    fn rebuild_projection(&mut self) -> bool {
        self.reset_projection();
        self.pending
            .iter()
            .all(|pending| self.projection.push(pending.edit))
    }

    fn software_cursor(&self) -> Option<SoftwareCursor<'_>> {
        if self.cursor_visible {
            return None;
        }
        let index = usize::from(self.x - self.geometry.x);
        let cell = self.row.get(index)?;
        let padding = self.row.get(index + 1)?;
        // A hidden hardware cursor can anchor an application-painted blank caret.
        // Its style may already be resolved into colors rather than REVERSED.
        // This is only a candidate: observe must prove that this exact style moves
        // with echoed text, leaving ordinary padding style behind.
        (blank_cell(cell) && blank_cell(padding) && cell != padding)
            .then_some(SoftwareCursor { cell, padding })
    }

    fn matches_context(&self, surface: &PaneSurfaceFrame, row: &EligibleRow<'_>) -> bool {
        // Global projection revisions also change for unrelated sidebar metadata.
        // Actual pane identity, focus (checked by eligible_row), modes and geometry
        // define the input context; the row comparison below validates the prompt.
        self.boot_id == surface.boot_id
            && self.geometry == row.geometry
            && self.terminal_modes == row.terminal_modes
            && self.cursor_visible == row.cursor_visible
            && self.surface_size == (surface.frame.width, surface.frame.height)
            && self.y == row.y
    }

    fn visible(&self) -> bool {
        self.trained
            && !self.expired
            && !self.pending.is_empty()
            && (self.click_trained
                || !self
                    .pending
                    .iter()
                    .any(|pending| matches!(pending.edit, Edit::ClickPosition(_))))
    }

    fn deadline(&self) -> Option<Instant> {
        // Only outstanding input can time out. Pausing at a confirmed, unchanged
        // prompt must not turn the next character into another network round trip.
        self.word_learning
            .as_ref()
            .map(|learning| learning.sent_at + PREDICTION_LIFETIME)
            .or_else(|| {
                self.pending.front().map(|pending| {
                    pending.sent_at
                        + if self.expired {
                            LATE_ECHO_LIFETIME
                        } else {
                            PREDICTION_LIFETIME
                        }
                })
            })
    }
}

impl InputPrediction {
    pub(super) fn reconnect_anchor(
        &self,
        surface: &PaneSurfaceFrame,
        pane_id: &str,
        agent: crate::detect::Agent,
    ) -> Option<(super::reconnect_draft::DraftAnchor, bool)> {
        let row = eligible_row(surface, pane_id)?;
        let start = agent_prompt_start(row.cells, agent)?;
        let cursor = usize::from(row.x - row.geometry.x);
        if cursor < start {
            return None;
        }
        let end = row.cells[start..]
            .iter()
            .rposition(|cell| cell.symbol != " ")
            .map_or(start, |index| start + index + 1)
            .max(cursor);
        let confirmed = self.line.as_ref().is_some_and(|line| {
            line.pane_id == pane_id
                && line.matches_context(surface, &row)
                && line.trained
                && line.pending.is_empty()
                && line.word_learning.is_none()
                && line.x == row.x
                && line.row == row.cells
                && usize::from(line.input_end - line.geometry.x) == end
        });
        Some((
            super::reconnect_draft::DraftAnchor {
                boot_id: surface.boot_id.clone(),
                agent,
                geometry: row.geometry,
                surface_size: (surface.frame.width, surface.frame.height),
                terminal_modes: row.terminal_modes,
                cursor_visible: row.cursor_visible,
                x: row.x,
                y: row.y,
                row: row.cells.to_vec(),
                input_start: start,
                input_end: end,
            },
            confirmed,
        ))
    }

    /// Only the draft model's exact, same-generation echo may restore this baseline.
    pub(super) fn adopt_reconnect_echo(
        &mut self,
        pane_id: &str,
        anchor: &super::reconnect_draft::DraftAnchor,
    ) {
        // Never overwrite an independently outstanding online input history.
        if self.line.is_some() {
            return;
        }
        self.set_agent_context(Some(anchor.agent));
        let word_rules = self
            .machine_context
            .as_ref()
            .filter(|_| self.profile_resume_epoch.is_none())
            .and_then(|machine| {
                self.profiles
                    .profile(machine, crate::detect::agent_label(anchor.agent))
            })
            .map_or_else(
                || std::array::from_fn(|_| WordRules::default()),
                |profile| profile.word_rules.clone(),
            );
        let mut line = PredictedLine {
            boot_id: anchor.boot_id.clone(),
            pane_id: pane_id.to_owned(),
            geometry: anchor.geometry,
            surface_size: anchor.surface_size,
            terminal_modes: anchor.terminal_modes,
            cursor_visible: anchor.cursor_visible,
            y: anchor.y,
            x: anchor.x,
            input_start: anchor.geometry.x + anchor.input_start as u16,
            input_end: anchor.geometry.x + anchor.input_end as u16,
            software_style: None,
            row: anchor.row.clone(),
            initial_row: anchor.row.clone(),
            initial_empty: anchor.input_start == anchor.input_end,
            profile_published: false,
            profile_epoch: self.profiles.epoch(),
            placeholder: None,
            projection: EditProjection::default(),
            pending: VecDeque::new(),
            trained: true,
            expired: false,
            word_rules,
            word_learning: None,
            click_trained: false,
            last_click: None,
            mouse_down: false,
        };
        line.software_style = line
            .software_cursor()
            .map(|cursor| (cursor.cell.clone(), cursor.padding.clone()));
        // An exact text echo does not identify a painted caret over nonblank text.
        // Wait for ordinary input to establish its style rather than guessing it.
        if !line.cursor_visible && line.software_style.is_none() {
            return;
        }
        line.reset_projection();
        self.line = Some(line);
    }

    pub(crate) fn set_profiles(&mut self, profiles: Profiles) -> bool {
        if profiles.epoch() < self.profiles.epoch()
            || self
                .profile_resume_epoch
                .is_some_and(|minimum| profiles.epoch() < minimum)
        {
            return false;
        }
        self.profile_resume_epoch = None;
        let repaint = if self.profiles.epoch() != profiles.epoch() {
            self.clear()
        } else {
            false
        };
        self.profiles = profiles;
        repaint
    }

    pub(crate) fn pause_profiles_until_epoch(&mut self, epoch: u64) -> bool {
        self.profile_resume_epoch = Some(
            self.profile_resume_epoch
                .map_or(epoch, |old| old.max(epoch)),
        );
        self.profile_updates.clear();
        self.clear()
    }

    pub(crate) fn take_profile_updates(&mut self) -> Vec<ProfileUpdate> {
        std::mem::take(&mut self.profile_updates)
    }

    pub(super) fn set_machine_target(
        &mut self,
        endpoint: crate::client::endpoint::ClientEndpointId,
        target: &str,
    ) {
        self.machine_keys
            .insert(endpoint, super::prediction_profiles::machine_key(target));
    }

    pub(super) fn select_machine(
        &mut self,
        endpoint: &crate::client::endpoint::ClientEndpointId,
    ) -> bool {
        let next = self.machine_keys.get(endpoint).cloned();
        if self.machine_context == next {
            return false;
        }
        let repaint = self.clear();
        self.machine_context = next;
        repaint
    }

    fn learn_profile(&mut self) {
        let (Some(machine), Some(agent), Some(line)) = (
            &self.machine_context,
            self.agent_context,
            self.line.as_mut(),
        ) else {
            return;
        };
        if !line.trained
            || line.expired
            || line.profile_published
            || self.profile_resume_epoch.is_some()
        {
            return;
        }
        line.profile_published = true;
        let fingerprints = if line.initial_empty {
            prompt_fingerprint(
                &line.initial_row,
                line.input_start - line.geometry.x,
                line.cursor_visible,
                line.terminal_modes,
            )
            .into_iter()
            .collect()
        } else {
            Vec::new()
        };
        let profile = EditorProfile {
            machine_key: machine.clone(),
            agent: crate::detect::agent_label(agent).to_owned(),
            word_rules: line.word_rules.clone(),
            echo_trained: true,
            prompt_fingerprints: fingerprints,
        };
        let update = ProfileUpdate::Observe {
            epoch: line.profile_epoch,
            profile,
        };
        if self.profiles.apply(&update) {
            self.profile_updates.push(update);
        }
    }

    fn invalidate_profile(&mut self) {
        let (Some(machine), Some(agent)) = (&self.machine_context, self.agent_context) else {
            return;
        };
        let agent = crate::detect::agent_label(agent).to_owned();
        let epoch = self
            .line
            .as_ref()
            .map_or(self.profiles.epoch(), |line| line.profile_epoch);
        let update = ProfileUpdate::Invalidate {
            epoch,
            machine_key: machine.clone(),
            agent,
        };
        if self.profiles.apply(&update) {
            self.profile_updates.push(update);
        }
    }
    pub(super) fn set_agent_context(&mut self, agent: Option<crate::detect::Agent>) -> bool {
        if self.agent_context == agent {
            return false;
        }
        let repaint = self.clear();
        self.agent_context = agent;
        repaint
    }

    pub(super) fn submit(&mut self) -> bool {
        let Some(line) = self.line.take() else {
            return false;
        };
        let repaint = line.visible();
        self.warm_prompt =
            (self.agent_context.is_some() && line.trained && !line.expired && line.initial_empty)
                .then_some(WarmPrompt {
                    boot_id: line.boot_id,
                    pane_id: line.pane_id,
                    geometry: line.geometry,
                    surface_size: line.surface_size,
                    terminal_modes: line.terminal_modes,
                    cursor_visible: line.cursor_visible,
                    x: line.input_start,
                    y: line.y,
                    row: line.initial_row,
                    word_rules: line.word_rules,
                    click_trained: line.click_trained,
                });
        repaint
    }
    pub(super) fn has_pending(&self) -> bool {
        self.line.as_ref().is_some_and(|line| {
            !line.expired && (!line.pending.is_empty() || line.word_learning.is_some())
        })
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.line.as_ref().and_then(PredictedLine::deadline)
    }

    /// Returns whether removing the prediction needs a repaint.
    pub(super) fn clear(&mut self) -> bool {
        self.warm_prompt = None;
        self.line.take().is_some_and(|line| line.visible())
    }

    pub(super) fn expire(&mut self, now: Instant) -> bool {
        if self.deadline().is_some_and(|deadline| now >= deadline) {
            tracing::debug!("remote input prediction expired");
            let Some(line) = self.line.as_mut() else {
                return false;
            };
            if line.expired || line.word_learning.is_some() {
                return self.clear();
            }
            let repaint = line.visible();
            line.expired = true;
            repaint
        } else {
            false
        }
    }

    /// Reconcile against an accepted full surface or an already-applied surface patch.
    /// Surface revisions are not input acknowledgments: unchanged rows can arrive while
    /// input is still travelling, and must not erase predictions prematurely.
    pub(super) fn observe(&mut self, surface: &PaneSurfaceFrame, now: Instant) -> bool {
        let repaint = self.expire(now);
        let Some(line) = self.line.as_mut() else {
            return repaint;
        };
        let Some(row) = eligible_row(surface, &line.pane_id) else {
            return self.clear() || repaint;
        };
        if !line.matches_context(surface, &row) {
            return self.clear() || repaint;
        }
        if row.x == line.x && row.cells == line.row.as_slice() {
            // Insert/delete can return to the original screen. With no input ACKs,
            // an unchanged screen cannot prove that either edit reached the server.
            return repaint;
        }
        if let Some(learning) = line.word_learning.take() {
            let mut matched = None;
            for start in learning.starts {
                line.reset_projection();
                let count = learning.before[start..].chars().count();
                if line.projection.push(Edit::WordBackspace(count))
                    && usize::from(row.x - row.geometry.x) == line.projection.x
                    && line.projection.matches(row.cells)
                {
                    matched = Some(start);
                    break;
                }
            }
            let Some(start) = matched else {
                if line.word_rules[learning.gesture.index()].is_trained() {
                    self.invalidate_profile();
                }
                return self.clear() || repaint;
            };
            if !line.word_rules[learning.gesture.index()].learn(&learning.before, start) {
                return self.clear() || repaint;
            }
            line.row.clone_from_slice(row.cells);
            line.x = row.x;
            line.input_end = row.geometry.x + line.projection.input_end as u16;
            line.placeholder = None;
            line.profile_published = false;
            line.reset_projection();
            self.learn_profile();
            return repaint;
        }
        let was_visible = line.visible();
        line.reset_projection();
        let mut confirmed = None;
        for (index, pending) in line.pending.iter().enumerate() {
            if !line.projection.push(pending.edit) {
                break;
            }
            if usize::from(row.x - row.geometry.x) == line.projection.x
                && line.projection.matches(row.cells)
            {
                // Retire the earliest exact prefix. Repeated insert/delete cycles
                // can have identical screens; later edits remain speculative.
                confirmed = Some(index + 1);
                break;
            }
        }
        let Some(confirmed) = confirmed else {
            if line.trained && !line.pending.is_empty() {
                self.invalidate_profile();
            }
            return self.clear() || repaint;
        };
        // Blank echo alone cannot establish permission to display characters.
        line.trained |= line
            .pending
            .iter()
            .take(confirmed)
            .any(|pending| matches!(pending.edit, Edit::Insert(character) if character != ' '));
        line.click_trained |= line
            .pending
            .iter()
            .take(confirmed)
            .any(|pending| matches!(pending.edit, Edit::ClickPosition(_)));
        line.pending.drain(..confirmed);
        line.row.clone_from_slice(row.cells);
        line.x = row.x;
        line.input_end = row.geometry.x + line.projection.input_end as u16;
        line.placeholder = None;
        if line.pending.is_empty() {
            line.expired = false;
        }
        if !line.rebuild_projection() {
            return self.clear() || repaint;
        }
        tracing::debug!(
            confirmed,
            pending = line.pending.len(),
            "remote input prediction confirmed"
        );
        let repaint = repaint || was_visible || line.visible();
        self.learn_profile();
        repaint
    }

    /// Record already-routed single-cell text, Backspace or horizontal motion. Other edits and
    /// controls invalidate confidence; releases have no echo.
    pub(super) fn record_input(
        &mut self,
        surface: &PaneSurfaceFrame,
        pane_id: &str,
        event: &ClientPaneInputEvent,
        now: Instant,
    ) -> bool {
        if matches!(
            event,
            ClientPaneInputEvent::Key {
                kind: ClientKeyKind::Release,
                ..
            }
        ) {
            return self.expire(now);
        }
        if matches!(
            event,
            ClientPaneInputEvent::Key {
                code: ClientKeyCode::Enter,
                modifiers: 0,
                kind: ClientKeyKind::Press,
                repeat_count: 0 | 1,
                generated_text: None,
                ..
            }
        ) {
            return self.submit();
        }
        if matches!(
            event,
            ClientPaneInputEvent::Mouse {
                kind: ClientMouseKind::Moved,
                modifiers: 0,
                position: ClientMousePosition::Cell { .. },
                ..
            }
        ) {
            return self.observe(surface, now);
        }
        if matches!(
            event,
            ClientPaneInputEvent::Mouse {
                kind: ClientMouseKind::Up(ClientMouseButton::Left),
                modifiers: 0,
                position: ClientMousePosition::Cell { .. },
                ..
            }
        ) {
            let repaint = self.observe(surface, now);
            if let Some(line) = self.line.as_mut().filter(|line| line.mouse_down) {
                line.mouse_down = false;
                return repaint;
            }
            return self.clear() || repaint;
        }
        let text = printable_text(event);
        let simple = simple_edit(event);
        let word = word_gesture(event);
        let click = match event {
            ClientPaneInputEvent::Mouse {
                kind: ClientMouseKind::Down(ClientMouseButton::Left),
                position: ClientMousePosition::Cell { column, row },
                modifiers: 0,
                ..
            } => Some((*column, *row)),
            _ => None,
        };
        let count = match text.as_ref() {
            Some(text) => text.chars().count(),
            None if word.is_some() || click.is_some() => 1,
            None => match simple {
                Some((_, count)) => count,
                None => return self.clear(),
            },
        };
        let mut repaint = self.observe(surface, now);
        if self
            .line
            .as_ref()
            .is_some_and(|line| line.expired || line.word_learning.is_some())
        {
            repaint |= self.clear();
        }
        if self
            .line
            .as_ref()
            .is_some_and(|line| line.pane_id != pane_id)
        {
            repaint |= self.clear();
        }
        let Some(row) = eligible_row(surface, pane_id) else {
            return self.clear() || repaint;
        };
        if self.line.is_none() {
            let relative_x = usize::from(row.x - row.geometry.x);
            let prompt_start = self
                .agent_context
                .and_then(|agent| agent_prompt_start(row.cells, agent));
            let input_start = prompt_start
                .filter(|start| {
                    *start <= relative_x
                        && row.cells[*start..relative_x].iter().all(single_cell_symbol)
                })
                .unwrap_or(relative_x);
            let initial_empty = prompt_start == Some(relative_x);
            let mut placeholder = None;
            let mut input_end = relative_x;
            if prompt_start.is_some() && !blank_cell(&row.cells[relative_x]) && row.cursor_visible {
                if let Some(end) = row
                    .cells
                    .iter()
                    .rposition(|cell| !blank_cell(cell))
                    .map(|end| end + 1)
                    .filter(|end| *end < row.cells.len())
                {
                    if row.cells[input_start..end].iter().all(single_cell_symbol) {
                        if initial_empty {
                            placeholder = Some((end, row.cells[end].clone()));
                        } else {
                            input_end = end;
                        }
                    }
                }
            }
            let mut line = PredictedLine {
                boot_id: surface.boot_id.clone(),
                pane_id: pane_id.to_owned(),
                geometry: row.geometry,
                surface_size: (surface.frame.width, surface.frame.height),
                terminal_modes: row.terminal_modes,
                cursor_visible: row.cursor_visible,
                y: row.y,
                x: row.x,
                input_start: row.geometry.x + input_start as u16,
                input_end: row.geometry.x + input_end as u16,
                software_style: None,
                row: row.cells.to_vec(),
                initial_row: row.cells.to_vec(),
                initial_empty,
                profile_published: false,
                profile_epoch: self.profiles.epoch(),
                placeholder,
                projection: EditProjection::default(),
                pending: VecDeque::new(),
                trained: false,
                expired: false,
                word_rules: std::array::from_fn(|_| WordRules::default()),
                word_learning: None,
                click_trained: false,
                last_click: None,
                mouse_down: false,
            };
            if let (Some(machine), Some(agent), None) = (
                &self.machine_context,
                self.agent_context,
                self.profile_resume_epoch,
            ) {
                if let Some(profile) = self
                    .profiles
                    .profile(machine, crate::detect::agent_label(agent))
                {
                    line.word_rules = profile.word_rules.clone();
                    line.trained = initial_empty
                        && profile.echo_trained
                        && prompt_fingerprint(
                            row.cells,
                            input_start as u16,
                            row.cursor_visible,
                            row.terminal_modes,
                        )
                        .is_some_and(|fingerprint| {
                            profile.prompt_fingerprints.contains(&fingerprint)
                        });
                }
            }
            if let Some(warm) = self.warm_prompt.take().filter(|warm| {
                self.agent_context.is_some()
                    && warm.boot_id == line.boot_id
                    && warm.pane_id == line.pane_id
                    && warm.geometry == line.geometry
                    && warm.surface_size == line.surface_size
                    && warm.terminal_modes == line.terminal_modes
                    && warm.cursor_visible == line.cursor_visible
                    && warm.x == row.x
                    && warm.y == row.y
                    && warm.row == row.cells
            }) {
                line.trained = true;
                for (rules, observed) in line.word_rules.iter_mut().zip(&warm.word_rules) {
                    rules.merge_observation(observed);
                }
                line.click_trained = warm.click_trained;
            }
            line.software_style = line
                .software_cursor()
                .map(|cursor| (cursor.cell.clone(), cursor.padding.clone()));
            line.reset_projection();
            self.line = Some(line);
        }
        let Some(line) = self.line.as_mut() else {
            return repaint;
        };
        if line.pending.len().saturating_add(count) > MAX_PENDING_EDITS {
            return self.clear() || repaint;
        }
        let was_visible = line.visible();
        let edits: Vec<_> = match (text, word, click) {
            (Some(text), _, _) => text.chars().map(Edit::Insert).collect(),
            (_, Some(gesture), _) => {
                if !line.trained || !line.projection.safe_segment() {
                    return self.clear() || repaint;
                }
                let before = line.projection.prefix();
                let rules = &line.word_rules[gesture.index()];
                if let Some(start) = rules.agreed_start(&before).filter(|start| *start > 0) {
                    debug_assert!(rules.is_trained());
                    vec![Edit::WordBackspace(before[start..].chars().count())]
                } else {
                    if !line.pending.is_empty() {
                        return self.clear() || repaint;
                    }
                    let starts: Vec<_> = rules
                        .candidate_starts(&before)
                        .into_iter()
                        .filter(|start| *start > 0 && *start < before.len())
                        .collect();
                    if starts.is_empty() {
                        return self.clear() || repaint;
                    }
                    line.word_learning = Some(WordLearning {
                        gesture,
                        before,
                        starts,
                        sent_at: now,
                    });
                    return repaint;
                }
            }
            (_, _, Some((column, y))) => {
                if !line.cursor_visible
                    || !line.terminal_modes.1
                    || y != line.y - line.geometry.y
                    || line.last_click.is_some_and(|last| {
                        now.saturating_duration_since(last) < MULTI_CLICK_WINDOW
                    })
                {
                    return self.clear() || repaint;
                }
                line.last_click = Some(now);
                line.mouse_down = true;
                vec![Edit::ClickPosition(usize::from(column))]
            }
            _ => {
                let Some((edit, _)) = simple else {
                    return self.clear() || repaint;
                };
                vec![edit; count]
            }
        };
        for edit in edits {
            if matches!(edit, Edit::End) && line.projection.x == line.projection.input_end {
                continue;
            }
            if !line.projection.push(edit) {
                return self.clear() || repaint;
            }
            line.pending.push_back(PendingEdit { edit, sent_at: now });
        }
        repaint || was_visible || line.visible()
    }

    /// Apply the cached edit projection to the composed frame only. No terminal
    /// state is changed and rendering does not allocate a projection per pane.
    pub(super) fn apply(&self, frame: &mut FrameData, origin: (u16, u16)) {
        let Some(line) = self.line.as_ref().filter(|line| line.visible()) else {
            return;
        };
        let (Some(base_x), Some(row_x), Some(y)) = (
            line.x.checked_add(origin.0),
            line.geometry.x.checked_add(origin.0),
            line.y.checked_add(origin.1),
        ) else {
            return;
        };
        let end = usize::from(row_x) + line.projection.cells.len();
        if end > usize::from(frame.width) || y >= frame.height {
            return;
        }
        let Some(cursor) = frame.cursor.as_ref() else {
            return;
        };
        if cursor.x != base_x || cursor.y != y {
            return;
        }
        let start = usize::from(y) * usize::from(frame.width) + usize::from(row_x);
        let Some(cells) = frame
            .cells
            .get_mut(start..start + line.projection.cells.len())
        else {
            return;
        };
        for ((cell, projected), change) in cells
            .iter_mut()
            .zip(&line.projection.cells)
            .zip(&line.projection.changes)
        {
            if *change != CellEdit::Unchanged {
                cell.clone_from(projected);
                if *change == CellEdit::Insert {
                    cell.modifier |= ratatui::style::Modifier::UNDERLINED.bits();
                }
            }
        }
        if let Some(cursor) = frame.cursor.as_mut() {
            cursor.x = row_x + line.projection.x as u16;
        }
    }
}

fn word_gesture(event: &ClientPaneInputEvent) -> Option<WordGesture> {
    let ClientPaneInputEvent::Key {
        code,
        modifiers,
        kind: ClientKeyKind::Press | ClientKeyKind::Repeat,
        repeat_count: 0 | 1,
        generated_text: None,
        ..
    } = event
    else {
        return None;
    };
    match code {
        ClientKeyCode::Char('w')
            if *modifiers == crossterm::event::KeyModifiers::CONTROL.bits() =>
        {
            Some(WordGesture::ControlW)
        }
        ClientKeyCode::Backspace if *modifiers == crossterm::event::KeyModifiers::ALT.bits() => {
            Some(WordGesture::AltBackspace)
        }
        _ => None,
    }
}

fn simple_edit(event: &ClientPaneInputEvent) -> Option<(Edit, usize)> {
    match event {
        ClientPaneInputEvent::Key {
            code,
            modifiers: 0,
            kind: ClientKeyKind::Press | ClientKeyKind::Repeat,
            repeat_count,
            generated_text: None,
            ..
        } if usize::from(*repeat_count) <= MAX_PENDING_EDITS => {
            let edit = match code {
                ClientKeyCode::Backspace => Edit::Backspace,
                ClientKeyCode::Left => Edit::Left,
                ClientKeyCode::Right => Edit::Right,
                ClientKeyCode::End => Edit::End,
                ClientKeyCode::Delete => Edit::Delete,
                _ => return None,
            };
            Some((edit, usize::from((*repeat_count).max(1))))
        }
        _ => None,
    }
}

fn printable_text(event: &ClientPaneInputEvent) -> Option<String> {
    let text = match event {
        ClientPaneInputEvent::TextCommit(text) if text.len() <= 256 => text.clone(),
        ClientPaneInputEvent::Key {
            code: ClientKeyCode::Char(character),
            modifiers,
            kind: ClientKeyKind::Press | ClientKeyKind::Repeat,
            repeat_count,
            generated_text,
            ..
        } if *modifiers & !crossterm::event::KeyModifiers::SHIFT.bits() == 0 => {
            if *repeat_count > 256 || generated_text.as_ref().is_some_and(|text| text.len() > 256) {
                return None;
            }
            let text = generated_text
                .clone()
                .unwrap_or_else(|| character.to_string());
            // OS repeat records are bounded before allocating; normal terminal repeats
            // arrive as independent events. Large/batched input remains authoritative.
            let repeats = usize::from((*repeat_count).max(1));
            if text.len().saturating_mul(repeats) > 256 {
                return None;
            }
            text.repeat(repeats)
        }
        _ => return None,
    };
    (!text.is_empty()
        && text.len() <= 256
        && text.chars().all(safe_character)
        && text.graphemes(true).count() == text.chars().count())
    .then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{CursorState, PaneSurfacePane, PaneSurfaceScrollMetrics};

    fn surface(text: &str) -> PaneSurfaceFrame {
        let mut frame = FrameData::from_ratatui_buffer(
            &ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 30, 4)),
            Some(CursorState {
                x: text.chars().count() as u16,
                y: 1,
                visible: true,
                shape: 2,
            }),
        );
        for (index, character) in text.chars().enumerate() {
            frame.cells[30 + index].symbol = character.to_string();
        }
        PaneSurfaceFrame {
            boot_id: "boot".into(),
            projection_revision: 1,
            surface_revision: 1,
            frame,
            panes: vec![PaneSurfacePane {
                pane_id: "pane".into(),
                content_revision: 2,
                rect: SurfaceRect {
                    x: 0,
                    y: 0,
                    width: 30,
                    height: 4,
                },
                inner_rect: SurfaceRect {
                    x: 0,
                    y: 0,
                    width: 30,
                    height: 4,
                },
                scrollbar_rect: None,
                scroll: None,
                focused: true,
                mouse_reporting: false,
                sgr_pixel_mouse: false,
                alternate_screen_active: false,
                pixel_width: 0,
                pixel_height: 0,
            }],
            splits: vec![],
            popup: None,
            graphics: Default::default(),
        }
    }

    fn key(code: ClientKeyCode) -> ClientPaneInputEvent {
        ClientPaneInputEvent::Key {
            code,
            modifiers: 0,
            kind: ClientKeyKind::Press,
            repeat_count: 1,
            shifted_codepoint: None,
            generated_text: None,
            tracks_release: false,
            physical_key_id: None,
            windows_record: None,
        }
    }

    fn record(
        prediction: &mut InputPrediction,
        surface: &PaneSurfaceFrame,
        text: &str,
        now: Instant,
    ) -> bool {
        prediction.record_input(
            surface,
            "pane",
            &ClientPaneInputEvent::TextCommit(text.into()),
            now,
        )
    }

    fn rendered(prediction: &InputPrediction, surface: &PaneSurfaceFrame) -> FrameData {
        let mut frame = surface.frame.clone();
        prediction.apply(&mut frame, (0, 0));
        frame
    }

    fn row_text(frame: &FrameData) -> String {
        frame.cells[30..60]
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<String>()
    }

    fn trained(now: Instant) -> (InputPrediction, PaneSurfaceFrame) {
        let mut prediction = InputPrediction::default();
        record(&mut prediction, &surface("$ "), "a", now);
        let echoed = surface("$ a");
        prediction.observe(&echoed, now + Duration::from_millis(100));
        (prediction, echoed)
    }

    // Pi hides the hardware cursor and paints a blank at its coordinates. The
    // terminal renderer resolves reverse video into colors before serialization.
    fn software_cursor_surface(text: &str) -> PaneSurfaceFrame {
        let mut surface = surface(text);
        surface.frame.cursor.as_mut().unwrap().visible = false;
        let cell = &mut surface.frame.cells[30 + text.chars().count()];
        cell.fg = 0x02_00_00_00;
        cell.bg = 0x02_ff_ff_ff;
        surface
    }

    #[test]
    fn prediction_learns_moving_software_cursor_and_moves_it_through_partial_echo() {
        let now = Instant::now();
        let mut prediction = InputPrediction::default();
        let initial = software_cursor_surface("");
        assert!(!record(&mut prediction, &initial, "a", now));
        let echoed = software_cursor_surface("a");
        prediction.observe(&echoed, now + Duration::from_millis(100));
        assert!(record(
            &mut prediction,
            &echoed,
            "bc",
            now + Duration::from_millis(110)
        ));
        let frame = rendered(&prediction, &echoed);
        assert!(row_text(&frame).starts_with("abc "));
        for index in 31..33 {
            assert_eq!(frame.cells[index].fg, 0);
            assert_eq!(frame.cells[index].bg, 0);
            assert_eq!(
                frame.cells[index].modifier,
                ratatui::style::Modifier::UNDERLINED.bits()
            );
        }
        assert_eq!(frame.cells[33], echoed.frame.cells[31]);
        assert!(!frame.cursor.as_ref().unwrap().visible);
        assert_eq!(frame.cursor.as_ref().unwrap().x, 3);
        assert_eq!(echoed.frame.cells[31].symbol, " ");
        let partial = software_cursor_surface("ab");
        prediction.observe(&partial, now + Duration::from_millis(150));
        assert_eq!(
            rendered(&prediction, &partial).cells[32..34],
            frame.cells[32..34]
        );
        let complete = software_cursor_surface("abc");
        prediction.observe(&complete, now + Duration::from_millis(200));
        assert!(!prediction.has_pending());
        assert_eq!(rendered(&prediction, &complete), complete.frame);
        assert!(record(
            &mut prediction,
            &complete,
            "d",
            now + Duration::from_secs(2)
        ));
        assert!(prediction.expire(now + Duration::from_secs(3)));
        assert_eq!(rendered(&prediction, &complete), complete.frame);
    }

    #[test]
    fn software_cursor_prediction_rejects_non_cursor_style_changes_and_wrong_echo() {
        let now = Instant::now();
        let changes: [fn(&mut PaneSurfaceFrame); 4] = [
            |surface| surface.frame.cells[30].symbol = "!".into(),
            |surface| surface.frame.cells[30].fg = 5,
            |surface| surface.frame.cells[31].bg = 5,
            |surface| surface.frame.cells[35].fg = 5,
        ];
        for change in changes {
            let mut prediction = InputPrediction::default();
            record(&mut prediction, &software_cursor_surface(""), "a", now);
            let mut echoed = software_cursor_surface("a");
            change(&mut echoed);
            prediction.observe(&echoed, now + Duration::from_millis(100));
            assert!(!prediction.has_pending());
            assert_eq!(rendered(&prediction, &echoed), echoed.frame);
        }
    }

    #[test]
    fn software_cursor_without_echo_never_reveals_input() {
        let now = Instant::now();
        let prompt = software_cursor_surface("");
        let mut prediction = InputPrediction::default();
        record(&mut prediction, &prompt, "secret", now);
        prediction.observe(&prompt, now + Duration::from_millis(100));
        assert_eq!(rendered(&prediction, &prompt), prompt.frame);
        let mut moved = software_cursor_surface("      ");
        moved.frame.cursor.as_mut().unwrap().x = 6;
        prediction.observe(&moved, now + Duration::from_millis(200));
        assert!(!prediction.has_pending());
        assert_eq!(rendered(&prediction, &moved), moved.frame);
    }

    #[test]
    fn prediction_waits_for_echo_then_displays_pending_suffix_without_mutating_authority() {
        let now = Instant::now();
        let mut prediction = InputPrediction::default();
        let initial = surface("$ ");
        assert!(!record(&mut prediction, &initial, "abc", now));
        assert_eq!(rendered(&prediction, &initial), initial.frame);

        let echoed = surface("$ a");
        assert!(prediction.observe(&echoed, now + Duration::from_millis(100)));
        let frame = rendered(&prediction, &echoed);
        assert!(row_text(&frame).starts_with("$ abc"));
        assert_eq!(frame.cursor.as_ref().unwrap().x, 5);
        assert_eq!(echoed.frame.cells[33].symbol, " ");
        assert_eq!(
            frame.cells[32].modifier & ratatui::style::Modifier::UNDERLINED.bits(),
            0
        );
        assert_ne!(
            frame.cells[33].modifier & ratatui::style::Modifier::UNDERLINED.bits(),
            0
        );
    }

    #[test]
    fn partial_and_complete_echo_retire_exactly_the_confirmed_prefix() {
        let now = Instant::now();
        let (mut prediction, echoed) = trained(now);
        assert!(record(
            &mut prediction,
            &echoed,
            "bcd",
            now + Duration::from_millis(110)
        ));
        let partial = surface("$ abc");
        prediction.observe(&partial, now + Duration::from_millis(200));
        assert!(row_text(&rendered(&prediction, &partial)).starts_with("$ abcd"));
        let complete = surface("$ abcd");
        prediction.observe(&complete, now + Duration::from_millis(300));
        assert!(!prediction.has_pending());
        assert_eq!(rendered(&prediction, &complete), complete.frame);
    }

    #[test]
    fn subsequent_input_uses_the_end_of_pending_text() {
        let now = Instant::now();
        let (mut prediction, echoed) = trained(now);
        record(
            &mut prediction,
            &echoed,
            "b",
            now + Duration::from_millis(110),
        );
        record(
            &mut prediction,
            &echoed,
            "c",
            now + Duration::from_millis(120),
        );
        assert!(row_text(&rendered(&prediction, &echoed)).starts_with("$ abc"));
    }

    #[test]
    fn unrelated_output_and_new_surface_revisions_do_not_count_as_input_acknowledgments() {
        let now = Instant::now();
        let (mut prediction, mut echoed) = trained(now);
        record(
            &mut prediction,
            &echoed,
            "b",
            now + Duration::from_millis(110),
        );
        echoed.surface_revision += 1;
        echoed.frame.cells[0].symbol = "!".into();
        prediction.observe(&echoed, now + Duration::from_millis(120));
        assert!(prediction.has_pending());
        assert!(row_text(&rendered(&prediction, &echoed)).starts_with("$ ab"));
    }

    #[test]
    fn mismatched_echo_rolls_back_and_requires_new_training() {
        let now = Instant::now();
        let (mut prediction, echoed) = trained(now);
        record(
            &mut prediction,
            &echoed,
            "b",
            now + Duration::from_millis(110),
        );
        let mismatch = surface("$ a!");
        assert!(prediction.observe(&mismatch, now + Duration::from_millis(120)));
        assert!(!prediction.has_pending());
        assert!(!record(
            &mut prediction,
            &mismatch,
            "c",
            now + Duration::from_millis(130)
        ));
        assert_eq!(rendered(&prediction, &mismatch), mismatch.frame);
    }

    #[test]
    fn pending_deadline_does_not_extend_with_more_input_or_unchanged_frames() {
        let now = Instant::now();
        let (mut prediction, echoed) = trained(now);
        let sent = now + Duration::from_millis(110);
        record(&mut prediction, &echoed, "b", sent);
        record(
            &mut prediction,
            &echoed,
            "c",
            sent + Duration::from_millis(500),
        );
        prediction.observe(&echoed, sent + Duration::from_millis(600));
        assert_eq!(prediction.deadline(), Some(sent + PREDICTION_LIFETIME));
        assert!(prediction.expire(sent + PREDICTION_LIFETIME));
        assert!(!prediction.has_pending());
        assert_eq!(rendered(&prediction, &echoed), echoed.frame);
    }

    #[test]
    fn non_echoing_password_input_never_appears_and_expires() {
        let now = Instant::now();
        let mut prediction = InputPrediction::default();
        let prompt = surface("Password: ");
        assert!(!record(&mut prediction, &prompt, "secret", now));
        prediction.observe(&prompt, now + Duration::from_millis(500));
        assert_eq!(rendered(&prediction, &prompt), prompt.frame);
        prediction.expire(now + PREDICTION_LIFETIME);
        assert!(!prediction.has_pending());
    }

    #[test]
    fn cursor_motion_over_blank_space_does_not_train_echo() {
        let now = Instant::now();
        let mut prediction = InputPrediction::default();
        record(&mut prediction, &surface("$ "), " ", now);
        let blank_echo = surface("$  ");
        prediction.observe(&blank_echo, now + Duration::from_millis(100));
        assert!(!record(
            &mut prediction,
            &blank_echo,
            "secret",
            now + Duration::from_millis(110)
        ));
        assert_eq!(rendered(&prediction, &blank_echo), blank_echo.frame);
    }

    #[test]
    fn controls_paste_and_complex_unicode_clear_pending_text_and_confidence() {
        let now = Instant::now();
        let mut control = key(ClientKeyCode::Char('r'));
        if let ClientPaneInputEvent::Key { modifiers, .. } = &mut control {
            *modifiers = crossterm::event::KeyModifiers::CONTROL.bits();
        }
        for event in [
            key(ClientKeyCode::Enter),
            key(ClientKeyCode::Up),
            control,
            ClientPaneInputEvent::Paste("secret".into()),
            ClientPaneInputEvent::TextCommit("界".into()),
        ] {
            let (mut prediction, echoed) = trained(now);
            record(
                &mut prediction,
                &echoed,
                "b",
                now + Duration::from_millis(110),
            );
            assert!(prediction.record_input(
                &echoed,
                "pane",
                &event,
                now + Duration::from_millis(120)
            ));
            assert!(!prediction.has_pending());
            assert!(!record(
                &mut prediction,
                &echoed,
                "c",
                now + Duration::from_millis(130)
            ));
        }
    }

    #[test]
    fn backspace_erases_confirmed_ascii_and_partial_echo_preserves_remaining_deletions() {
        let now = Instant::now();
        let mut prediction = InputPrediction::default();
        record(&mut prediction, &surface("$ "), "abc", now);
        let authoritative = surface("$ abc");
        prediction.observe(&authoritative, now);
        for _ in 0..2 {
            assert!(prediction.record_input(
                &authoritative,
                "pane",
                &key(ClientKeyCode::Backspace),
                now
            ));
        }
        let predicted = rendered(&prediction, &authoritative);
        assert!(row_text(&predicted).starts_with("$ a  "));
        assert_eq!(predicted.cursor.as_ref().unwrap().x, 3);
        assert!(row_text(&authoritative.frame).starts_with("$ abc"));
        let partial = surface("$ ab");
        prediction.observe(&partial, now);
        assert!(prediction.has_pending());
        assert!(row_text(&rendered(&prediction, &partial)).starts_with("$ a  "));
        let complete = surface("$ a");
        prediction.observe(&complete, now);
        assert!(!prediction.has_pending());
        assert_eq!(rendered(&prediction, &complete), complete.frame);
        assert!(record(&mut prediction, &complete, "b", now));
        assert!(row_text(&rendered(&prediction, &complete)).starts_with("$ ab"));
    }

    #[test]
    fn interleaved_typing_and_backspace_reconcile_each_exact_input_prefix() {
        let now = Instant::now();
        let (mut prediction, authoritative) = trained(now);
        record(&mut prediction, &authoritative, "bc", now);
        prediction.record_input(&authoritative, "pane", &key(ClientKeyCode::Backspace), now);
        record(&mut prediction, &authoritative, "d", now);
        assert!(row_text(&rendered(&prediction, &authoritative)).starts_with("$ abd "));
        for text in ["$ ab", "$ abc", "$ ab", "$ abd"] {
            let partial = surface(text);
            prediction.observe(&partial, now);
            assert!(row_text(&rendered(&prediction, &partial)).starts_with("$ abd "));
        }
        assert!(!prediction.has_pending());
    }

    #[test]
    fn unchanged_screen_cannot_acknowledge_an_insert_delete_cycle() {
        let now = Instant::now();
        let (mut prediction, authoritative) = trained(now);
        record(&mut prediction, &authoritative, "b", now);
        prediction.record_input(&authoritative, "pane", &key(ClientKeyCode::Backspace), now);
        prediction.observe(&authoritative, now);
        assert!(prediction.has_pending());
        assert_eq!(rendered(&prediction, &authoritative), authoritative.frame);
        record(&mut prediction, &authoritative, "c", now);
        let coalesced = surface("$ ac");
        prediction.observe(&coalesced, now);
        assert!(!prediction.has_pending());
        assert_eq!(rendered(&prediction, &coalesced), coalesced.frame);
    }

    #[test]
    fn backspace_never_predicts_deleting_the_prompt_or_unobserved_existing_text() {
        let now = Instant::now();
        let (mut prediction, authoritative) = trained(now);
        prediction.record_input(&authoritative, "pane", &key(ClientKeyCode::Backspace), now);
        assert!(row_text(&rendered(&prediction, &authoritative)).starts_with("$  "));
        prediction.record_input(&authoritative, "pane", &key(ClientKeyCode::Backspace), now);
        assert!(!prediction.has_pending());
        assert_eq!(rendered(&prediction, &authoritative), authoritative.frame);
        let mut prediction = InputPrediction::default();
        assert!(!prediction.record_input(
            &surface("$ old"),
            "pane",
            &key(ClientKeyCode::Backspace),
            now
        ));
        assert!(!prediction.has_pending());
    }

    #[test]
    fn modified_or_opaque_backspace_and_excessive_repeat_remain_authoritative() {
        let now = Instant::now();
        for modifiers in [
            crossterm::event::KeyModifiers::CONTROL,
            crossterm::event::KeyModifiers::ALT,
            crossterm::event::KeyModifiers::SHIFT,
        ] {
            let (mut prediction, authoritative) = trained(now);
            record(&mut prediction, &authoritative, "b", now);
            let mut event = key(ClientKeyCode::Backspace);
            if let ClientPaneInputEvent::Key {
                modifiers: bits, ..
            } = &mut event
            {
                *bits = modifiers.bits();
            }
            assert!(prediction.record_input(&authoritative, "pane", &event, now));
            assert!(!prediction.has_pending());
        }
        for opaque in [false, true] {
            let (mut prediction, authoritative) = trained(now);
            record(&mut prediction, &authoritative, "b", now);
            let mut event = key(ClientKeyCode::Backspace);
            if let ClientPaneInputEvent::Key {
                repeat_count,
                generated_text,
                ..
            } = &mut event
            {
                if opaque {
                    *generated_text = Some("unexpected".into());
                } else {
                    *repeat_count = MAX_PENDING_EDITS as u16 + 1;
                }
            }
            prediction.record_input(&authoritative, "pane", &event, now);
            assert!(!prediction.has_pending());
        }
    }

    #[test]
    fn backspace_mismatch_and_timeout_restore_authority() {
        let now = Instant::now();
        for mismatch in [false, true] {
            let (mut prediction, authoritative) = trained(now);
            prediction.record_input(&authoritative, "pane", &key(ClientKeyCode::Backspace), now);
            if mismatch {
                let mut wrong = authoritative.clone();
                wrong.frame.cursor.as_mut().unwrap().x -= 1;
                prediction.observe(&wrong, now);
                assert_eq!(rendered(&prediction, &wrong), wrong.frame);
            } else {
                assert!(prediction.expire(now + PREDICTION_LIFETIME));
                assert_eq!(rendered(&prediction, &authoritative), authoritative.frame);
            }
            assert!(!prediction.has_pending());
        }
    }

    #[test]
    fn backspace_moves_a_learned_software_caret_through_partial_confirmation() {
        let now = Instant::now();
        let mut prediction = InputPrediction::default();
        record(&mut prediction, &software_cursor_surface(""), "abc", now);
        let authoritative = software_cursor_surface("abc");
        prediction.observe(&authoritative, now);
        let mut event = key(ClientKeyCode::Backspace);
        if let ClientPaneInputEvent::Key { repeat_count, .. } = &mut event {
            *repeat_count = 2;
        }
        assert!(prediction.record_input(&authoritative, "pane", &event, now));
        let predicted = rendered(&prediction, &authoritative);
        assert!(row_text(&predicted).starts_with("a   "));
        assert_eq!(predicted.cells[31], authoritative.frame.cells[33]);
        assert_eq!(predicted.cursor.as_ref().unwrap().x, 1);
        let partial = software_cursor_surface("ab");
        prediction.observe(&partial, now);
        assert_eq!(rendered(&prediction, &partial), predicted);
        let complete = software_cursor_surface("a");
        prediction.observe(&complete, now);
        assert!(!prediction.has_pending());
        assert_eq!(rendered(&prediction, &complete), complete.frame);
    }

    #[test]
    fn insert_delete_cycles_have_a_bounded_pending_queue() {
        let now = Instant::now();
        let (mut prediction, authoritative) = trained(now);
        for _ in 0..MAX_PENDING_EDITS / 2 {
            record(&mut prediction, &authoritative, "b", now);
            prediction.record_input(&authoritative, "pane", &key(ClientKeyCode::Backspace), now);
        }
        assert_eq!(
            prediction.line.as_ref().unwrap().pending.len(),
            MAX_PENDING_EDITS
        );
        record(&mut prediction, &authoritative, "b", now);
        assert!(!prediction.has_pending());
    }

    #[test]
    fn context_changes_clear_predictions() {
        let now = Instant::now();
        let changes: [fn(&mut PaneSurfaceFrame); 7] = [
            |surface| surface.boot_id = "new-boot".into(),
            |surface| surface.panes[0].focused = false,
            |surface| surface.panes[0].alternate_screen_active = true,
            |surface| surface.panes[0].mouse_reporting = true,
            |surface| surface.frame.cursor.as_mut().unwrap().visible = false,
            |surface| surface.panes[0].inner_rect.width -= 1,
            |surface| {
                surface.panes[0].scroll = Some(PaneSurfaceScrollMetrics {
                    offset_from_bottom: 1,
                    max_offset_from_bottom: 10,
                    viewport_rows: 4,
                })
            },
        ];
        for change in changes {
            let (mut prediction, mut echoed) = trained(now);
            record(
                &mut prediction,
                &echoed,
                "b",
                now + Duration::from_millis(110),
            );
            change(&mut echoed);
            assert!(prediction.observe(&echoed, now + Duration::from_millis(120)));
            assert!(!prediction.has_pending());
        }
    }

    #[test]
    fn occupied_cells_and_terminal_right_edge_are_never_predicted() {
        let now = Instant::now();
        let (mut prediction, mut echoed) = trained(now);
        echoed.frame.cells[34].symbol = "!".into();
        assert!(!record(
            &mut prediction,
            &echoed,
            "bc",
            now + Duration::from_millis(110)
        ));
        assert!(!prediction.has_pending());
        let initial = surface(&"x".repeat(28));
        let mut prediction = InputPrediction::default();
        assert!(!record(&mut prediction, &initial, "ab", now));
        assert!(!prediction.has_pending());
    }

    #[test]
    fn prediction_offsets_cells_and_cursor_into_composed_frame() {
        let now = Instant::now();
        let (mut prediction, echoed) = trained(now);
        record(
            &mut prediction,
            &echoed,
            "b",
            now + Duration::from_millis(110),
        );
        let mut frame = FrameData::from_ratatui_buffer(
            &ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 40, 8)),
            Some(CursorState {
                x: 8,
                y: 3,
                visible: true,
                shape: 2,
            }),
        );
        prediction.apply(&mut frame, (5, 2));
        assert_eq!(frame.cells[3 * 40 + 8].symbol, "b");
        assert_eq!(frame.cursor.unwrap().x, 9);
    }

    #[test]
    fn key_release_and_idle_pause_preserve_confirmed_prompt_confidence() {
        let now = Instant::now();
        let (mut prediction, echoed) = trained(now);
        let mut release = key(ClientKeyCode::Char('a'));
        if let ClientPaneInputEvent::Key { kind, .. } = &mut release {
            *kind = ClientKeyKind::Release;
        }
        assert!(!prediction.record_input(
            &echoed,
            "pane",
            &release,
            now + Duration::from_millis(110)
        ));
        assert!(record(
            &mut prediction,
            &echoed,
            "b",
            now + Duration::from_millis(120)
        ));
        assert!(!prediction.record_input(
            &echoed,
            "pane",
            &release,
            now + Duration::from_millis(130)
        ));
        assert!(prediction.has_pending());
        let (mut prediction, echoed) = trained(now);
        assert_eq!(prediction.deadline(), None);
        prediction.expire(now + Duration::from_millis(100) + PREDICTION_LIFETIME);
        assert!(record(
            &mut prediction,
            &echoed,
            "b",
            now + Duration::from_secs(1)
        ));
    }

    #[test]
    fn semantic_key_presses_and_repeat_counts_predict_exact_echoed_text() {
        let now = Instant::now();
        let (mut prediction, echoed) = trained(now);
        let mut event = key(ClientKeyCode::Char('b'));
        if let ClientPaneInputEvent::Key {
            kind, repeat_count, ..
        } = &mut event
        {
            *kind = ClientKeyKind::Repeat;
            *repeat_count = 3;
        }
        assert!(prediction.record_input(&echoed, "pane", &event, now + Duration::from_millis(110)));
        assert!(row_text(&rendered(&prediction, &echoed)).starts_with("$ abbb"));
        let complete = surface("$ abbb");
        prediction.observe(&complete, now + Duration::from_millis(120));
        assert_eq!(rendered(&prediction, &complete), complete.frame);
    }

    #[test]
    fn popup_or_missing_cursor_invalidates_prediction() {
        let now = Instant::now();
        for popup in [false, true] {
            let (mut prediction, mut echoed) = trained(now);
            record(
                &mut prediction,
                &echoed,
                "b",
                now + Duration::from_millis(110),
            );
            if popup {
                echoed.popup = Some(Box::new(crate::protocol::ClientShellPopupSurface {
                    terminal_id: "popup".into(),
                    title: "Popup".into(),
                    width: None,
                    height: None,
                    frame: echoed.frame.clone(),
                    mouse_reporting: false,
                    sgr_pixel_mouse: false,
                    pixel_width: 0,
                    pixel_height: 0,
                }));
            } else {
                echoed.frame.cursor = None;
            }
            assert!(prediction.observe(&echoed, now + Duration::from_millis(120)));
            assert!(!prediction.has_pending());
        }
    }

    #[test]
    fn cursor_movement_without_matching_characters_does_not_confirm_pending_input() {
        let now = Instant::now();
        let (mut prediction, mut echoed) = trained(now);
        record(
            &mut prediction,
            &echoed,
            "b",
            now + Duration::from_millis(110),
        );
        echoed.frame.cursor.as_mut().unwrap().x += 1;
        assert!(prediction.observe(&echoed, now + Duration::from_millis(120)));
        assert!(!prediction.has_pending());
    }

    include!("prediction_sequence_tests.rs");

    fn at_cursor(mut frame: PaneSurfaceFrame, x: u16) -> PaneSurfaceFrame {
        frame.frame.cursor.as_mut().unwrap().x = x;
        frame
    }

    #[test]
    fn ordered_middle_edits_compose_before_any_echo_and_reconcile_partial_prefixes() {
        let now = Instant::now();
        let (mut prediction, base) = trained(now);
        record(&mut prediction, &base, "bcdef", now);
        for code in [
            ClientKeyCode::Left,
            ClientKeyCode::Left,
            ClientKeyCode::Backspace,
        ] {
            assert!(prediction.record_input(&base, "pane", &key(code), now));
        }
        record(&mut prediction, &base, "é", now);
        prediction.record_input(&base, "pane", &key(ClientKeyCode::Right), now);
        prediction.record_input(&base, "pane", &key(ClientKeyCode::Delete), now);
        prediction.record_input(&base, "pane", &key(ClientKeyCode::End), now);
        record(&mut prediction, &base, "λ", now);
        let composed = rendered(&prediction, &base);
        assert!(row_text(&composed).starts_with("$ abcéeλ "));
        assert_eq!(composed.cursor.unwrap().x, 8);
        assert_eq!(row_text(&base.frame).trim_end(), "$ a");
        let partial = surface("$ abcdef");
        prediction.observe(&partial, now + Duration::from_millis(100));
        assert!(row_text(&rendered(&prediction, &partial)).starts_with("$ abcéeλ "));
        let moved = at_cursor(partial, 6);
        prediction.observe(&moved, now + Duration::from_millis(120));
        let complete = surface("$ abcéeλ");
        prediction.observe(&complete, now + Duration::from_millis(150));
        assert!(!prediction.has_pending());
        assert_eq!(rendered(&prediction, &complete), complete.frame);
    }

    #[test]
    fn cursor_boundaries_and_unknown_suffix_fall_back_without_changing_authority() {
        let now = Instant::now();
        let (mut prediction, base) = trained(now);
        prediction.record_input(&base, "pane", &key(ClientKeyCode::Left), now);
        assert_eq!(rendered(&prediction, &base).cursor.unwrap().x, 2);
        prediction.record_input(&base, "pane", &key(ClientKeyCode::Left), now);
        assert!(!prediction.has_pending());
        let (mut prediction, base) = trained(now);
        prediction.record_input(&base, "pane", &key(ClientKeyCode::Right), now);
        assert!(!prediction.has_pending());
        let mut occupied = base.clone();
        occupied.frame.cells[33].symbol = "?".into();
        record(&mut prediction, &occupied, "b", now);
        assert!(!prediction.has_pending());
    }

    #[test]
    fn unicode_is_exact_single_cell_text_with_complex_clusters_remaining_authoritative() {
        let now = Instant::now();
        let (mut prediction, base) = trained(now);
        record(&mut prediction, &base, "éλЖ", now);
        assert!(row_text(&rendered(&prediction, &base)).starts_with("$ aéλЖ"));
        let echoed = surface("$ aéλЖ");
        prediction.observe(&echoed, now + Duration::from_millis(100));
        prediction.record_input(&echoed, "pane", &key(ClientKeyCode::Backspace), now);
        assert!(row_text(&rendered(&prediction, &echoed)).starts_with("$ aéλ "));
        for unsupported in [
            "\u{301}",
            "e\u{301}",
            "界",
            "👩‍💻",
            "\u{200d}",
            "\u{202e}",
            "🇺🇸",
            "\u{fe0f}",
        ] {
            let (mut prediction, base) = trained(now);
            record(&mut prediction, &base, unsupported, now);
            assert!(!prediction.has_pending(), "{unsupported:?}");
            assert_eq!(rendered(&prediction, &base), base.frame);
        }
    }

    fn word_key(gesture: WordGesture) -> ClientPaneInputEvent {
        let (code, modifier) = match gesture {
            WordGesture::ControlW => (
                ClientKeyCode::Char('w'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
            WordGesture::AltBackspace => (
                ClientKeyCode::Backspace,
                crossterm::event::KeyModifiers::ALT,
            ),
        };
        let mut event = key(code);
        if let ClientPaneInputEvent::Key { modifiers, .. } = &mut event {
            *modifiers = modifier.bits();
        }
        event
    }

    #[test]
    fn each_word_gesture_learns_independently_and_predicts_only_agreeing_boundaries() {
        let now = Instant::now();
        let (mut prediction, base) = trained(now);
        record(&mut prediction, &base, " alpha tail", now);
        let full = surface("$ a alpha tail");
        prediction.observe(&full, now + Duration::from_millis(100));
        let control = word_key(WordGesture::ControlW);
        prediction.record_input(&full, "pane", &control, now + Duration::from_millis(110));
        assert_eq!(
            rendered(&prediction, &full),
            full.frame,
            "first word delete learns, without guessing"
        );
        let deleted = surface("$ a alpha ");
        prediction.observe(&deleted, now + Duration::from_millis(150));
        record(
            &mut prediction,
            &deleted,
            "other",
            now + Duration::from_millis(160),
        );
        prediction.record_input(&deleted, "pane", &control, now + Duration::from_millis(170));
        assert!(row_text(&rendered(&prediction, &deleted)).starts_with("$ a alpha  "));
        // Cancelling insert/delete remains pending until a differing screen proves a prefix.
        assert!(prediction.has_pending());
        record(
            &mut prediction,
            &deleted,
            "z",
            now + Duration::from_millis(180),
        );
        let coalesced = surface("$ a alpha z");
        prediction.observe(&coalesced, now + Duration::from_millis(200));
        assert!(!prediction.has_pending());
        prediction.record_input(
            &coalesced,
            "pane",
            &word_key(WordGesture::AltBackspace),
            now + Duration::from_millis(210),
        );
        assert_eq!(
            rendered(&prediction, &coalesced),
            coalesced.frame,
            "Alt-Backspace needs its own echo"
        );
    }

    #[test]
    fn late_exact_echo_recovers_observed_bounds_without_resurrecting_expired_prediction() {
        let now = Instant::now();
        let (mut prediction, base) = trained(now);
        record(&mut prediction, &base, "bc", now);
        assert!(prediction.expire(now + PREDICTION_LIFETIME));
        assert!(!prediction.has_pending());
        assert_eq!(rendered(&prediction, &base), base.frame);
        let partial = surface("$ ab");
        prediction.observe(&partial, now + Duration::from_secs(1));
        assert_eq!(rendered(&prediction, &partial), partial.frame);
        let complete = surface("$ abc");
        prediction.observe(&complete, now + Duration::from_millis(1100));
        prediction.record_input(
            &complete,
            "pane",
            &key(ClientKeyCode::Backspace),
            now + Duration::from_millis(1200),
        );
        prediction.record_input(
            &complete,
            "pane",
            &key(ClientKeyCode::Backspace),
            now + Duration::from_millis(1200),
        );
        assert!(row_text(&rendered(&prediction, &complete)).starts_with("$ a  "));
    }
}
