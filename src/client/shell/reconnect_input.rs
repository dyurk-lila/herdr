//! Client-owned outage drafts; remote input leases and authoritative frames stay unchanged.
use super::reconnect_draft::{DraftNotice, DraftTarget};
use super::*;
use crate::raw_input::RawInputEvent;
use crossterm::event::KeyEventKind;
use std::time::Instant;

pub(crate) struct ReconnectDraftInput {
    target: DraftTarget,
    pub(crate) request: ClientMessage,
}

pub(crate) struct ReconnectPanelUnderlay {
    area: Rect,
    cells: Vec<crate::protocol::CellData>,
    links: Vec<(u32, String)>,
}

impl ReconnectPanelUnderlay {
    pub(crate) fn clean_base(&self, source: &FrameData) -> FrameData {
        let mut frame = source.clone();
        frame.graphics.clear();
        let area = self.area;
        if area.right() <= frame.width && area.bottom() <= frame.height {
            let remap: Vec<_> = self
                .links
                .iter()
                .map(|(old, uri)| {
                    let index = frame
                        .hyperlinks
                        .iter()
                        .position(|candidate| candidate == uri)
                        .unwrap_or_else(|| {
                            frame.hyperlinks.push(uri.clone());
                            frame.hyperlinks.len() - 1
                        });
                    (*old, index as u32)
                })
                .collect();
            for (row, y) in (area.y..area.bottom()).enumerate() {
                let start = usize::from(y) * usize::from(frame.width) + usize::from(area.x);
                let offset = row * usize::from(area.width);
                frame.cells[start..start + usize::from(area.width)]
                    .clone_from_slice(&self.cells[offset..offset + usize::from(area.width)]);
                for cell in &mut frame.cells[start..start + usize::from(area.width)] {
                    cell.hyperlink = cell.hyperlink.and_then(|old| {
                        remap
                            .iter()
                            .find(|(source, _)| *source == old)
                            .map(|(_, new)| *new)
                    });
                }
            }
        }
        frame.cursor = None;
        frame
    }
}

impl ClientShellState {
    pub(crate) fn take_reconnect_underlay(&mut self) -> Option<ReconnectPanelUnderlay> {
        self.reconnect_panel_underlay.take()
    }

    pub(crate) fn has_reconnect_draft(&self) -> bool {
        self.reconnect_display_target().is_some()
    }

    fn reconnect_target(&self) -> Option<DraftTarget> {
        Some(DraftTarget {
            endpoint_id: self.active_endpoint_id.clone(),
            pane_id: self.focused_pane_id()?,
        })
    }

    pub(super) fn reconnect_display_target(&self) -> Option<DraftTarget> {
        if let Some(target) = self.reconnect_target() {
            if self.reconnect_drafts.view(&target).is_some() {
                return Some(target);
            }
        }
        self.reconnect_drafts
            .target_for_endpoint(&self.active_endpoint_id)
            .cloned()
    }

    fn reconnect_agent(&self, pane_id: &str) -> Option<crate::detect::Agent> {
        let agent = self
            .snapshot
            .as_ref()?
            .agents
            .iter()
            .find(|agent| agent.pane_id == pane_id)?;
        crate::detect::parse_agent_label(agent.agent.as_deref()?)
    }

    pub(super) fn begin_reconnect_draft(&mut self) {
        if !self.config.remote_buffer_reconnect_input
            || (!self.primary_remote && self.active_endpoint_id.is_local())
            || self.mode != ClientShellMode::Terminal
            || self.overlay.is_some()
            || self.popup_terminal_id.is_some()
            || self.popup_pending
        {
            return;
        }
        let Some(target) = self.reconnect_target() else {
            return;
        };
        if !self.reconnect_drafts.begin(&target) {
            self.set_endpoint_error("Reconnect scratchpad target limit reached");
        }
        self.reconnect_input_ready = false;
    }

    pub(crate) fn set_reconnect_input_ready(&mut self, ready: bool) {
        self.reconnect_input_ready = ready;
    }

    pub(crate) fn remove_empty_reconnect_drafts(&mut self) -> bool {
        self.reconnect_drafts.remove_empty(&self.active_endpoint_id)
    }

    pub(crate) fn take_reconnect_draft_input(&mut self) -> Option<ReconnectDraftInput> {
        if !self.reconnect_input_ready
            || self.active_snapshot_generation.is_none()
            || self.pending_pane_surface.is_some()
            || self.pane_surface_generation != self.active_snapshot_generation
            || self.overlay.is_some()
            || self.mode != ClientShellMode::Terminal
            || self.popup_terminal_id.is_some()
            || self.popup_pending
            || self
                .pane_surface
                .as_ref()
                .is_some_and(|surface| surface.popup.is_some())
        {
            return None;
        }
        let target = self.reconnect_display_target()?;
        let surface = self.pane_surface.as_ref()?;
        let pane_id = if surface
            .panes
            .iter()
            .any(|pane| pane.pane_id == target.pane_id)
        {
            target.pane_id.clone()
        } else {
            let focused = self.focused_pane_id()?;
            surface
                .panes
                .iter()
                .any(|pane| pane.pane_id == focused)
                .then_some(focused)?
        };
        let attempt = self.reconnect_drafts.attempt(&target)?;
        let mut events = vec![ClientPaneInputEvent::TextCommit(attempt.text)];
        // Large cursor restoration must not overflow the stable server's event batch.
        // The full text still transfers; exceptionally large movements leave the caret at end.
        let mut trailing_left = if attempt.trailing_left < crate::protocol::MAX_INPUT_EVENT_BATCH {
            attempt.trailing_left
        } else {
            0
        };
        while trailing_left > 0 {
            let repeat_count = trailing_left.min(usize::from(u16::MAX)) as u16;
            trailing_left -= usize::from(repeat_count);
            events.push(ClientPaneInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Left,
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,
                repeat_count,
                generated_text: None,
                shifted_codepoint: None,
                tracks_release: false,
                physical_key_id: None,
                windows_record: None,
            });
        }
        Some(ReconnectDraftInput {
            target,
            request: ClientMessage::ClientShellPaneInput { pane_id, events },
        })
    }

    /// Enqueue acceptance permanently removes only this never-sent scratchpad.
    pub(crate) fn commit_reconnect_draft_handoff(&mut self, input: &ReconnectDraftInput) -> bool {
        let changed = self.reconnect_drafts.commit(&input.target);
        if changed && self.prediction_allowed() {
            if let ClientMessage::ClientShellPaneInput { pane_id, events } = &input.request {
                self.input_prediction
                    .select_machine(&self.active_endpoint_id);
                self.input_prediction
                    .set_agent_context(self.reconnect_agent(pane_id));
                if let Some(surface) = self.pane_surface.as_ref() {
                    for event in events {
                        self.input_prediction
                            .record_input(surface, pane_id, event, Instant::now());
                    }
                }
            }
        }
        changed
    }

    pub(super) fn handle_reconnect_draft_event(
        &mut self,
        event: &RawInputEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(target) = self.reconnect_display_target() else {
            return false;
        };
        if let RawInputEvent::Mouse(mouse) = event {
            if contains(self.hits.reconnect_panel, (mouse.column, mouse.row)) {
                return true;
            }
        }
        if self.mode != ClientShellMode::Terminal
            || self.overlay.is_some()
            || self.copy_mode.is_some()
            || self.selection.is_some()
            || self.popup_pending
            || self.popup_terminal_id.is_some()
        {
            return false;
        }
        if let RawInputEvent::Key(key) = event {
            if key.kind == KeyEventKind::Release {
                return false;
            }
            if self.config.keybinds.matches_prefix(key)
                || crate::input::resolve_direct_binding(&self.config.keybinds.keybinds, key)
                    .is_some()
            {
                return false;
            }
        }
        let result = self.reconnect_drafts.edit(&target, event);
        outcome.repaint |= result.repaint;
        result.handled
    }

    /// Projects local draft cells only; no candidate remote surface or graphics cross the freeze.
    pub(crate) fn project_reconnect_draft(&mut self, source: &FrameData) -> Option<FrameData> {
        self.reconnect_panel_underlay = None;
        self.hits.reconnect_panel = Rect::default();
        self.reconnect_display_target()?;
        if self.mode != ClientShellMode::Terminal || self.overlay.is_some() {
            return None;
        }
        let mut frame = source.clone();
        frame.graphics.clear();
        let area = self.layout(frame.width, frame.height).pane_surface;
        let mut occlusion = crate::kitty_graphics::surface::Occlusion::default();
        self.render_reconnect_draft(&mut frame, area, &mut occlusion);
        Some(frame)
    }

    pub(super) fn render_reconnect_draft(
        &mut self,
        frame: &mut FrameData,
        pane_area: Rect,
        occlusion: &mut crate::kitty_graphics::surface::Occlusion,
    ) {
        self.reconnect_panel_underlay = None;
        if self.mode != ClientShellMode::Terminal
            || self.overlay.is_some()
            || pane_area.height < 2
            || pane_area.width < 24
        {
            return;
        }
        let Some(target) = self.reconnect_display_target() else {
            return;
        };
        let Some(view) = self.reconnect_drafts.view(&target) else {
            return;
        };
        let focused = self.focused_pane_id();
        let pane_area = self
            .pane_surface
            .as_ref()
            .and_then(|surface| {
                surface
                    .panes
                    .iter()
                    .find(|pane| Some(&pane.pane_id) == focused.as_ref())
            })
            .map(|pane| {
                Rect::new(
                    pane_area.x.saturating_add(pane.inner_rect.x),
                    pane_area.y.saturating_add(pane.inner_rect.y),
                    pane.inner_rect.width,
                    pane.inner_rect.height,
                )
                .intersection(pane_area)
            })
            .filter(|area| area.width >= 24 && area.height >= 2)
            .unwrap_or(pane_area);
        let area = Rect::new(pane_area.x, pane_area.bottom() - 2, pane_area.width, 2);
        self.hits.reconnect_panel = area;
        let Some(mut buffer) = frame.to_ratatui_buffer() else {
            return;
        };
        let underlay_cells: Vec<_> = (area.y..area.bottom())
            .flat_map(|y| {
                let start = usize::from(y) * usize::from(frame.width) + usize::from(area.x);
                frame.cells[start..start + usize::from(area.width)]
                    .iter()
                    .cloned()
            })
            .collect();
        let mut links = Vec::new();
        for cell in &underlay_cells {
            if let Some(index) = cell.hyperlink {
                if links.iter().any(|(saved, _)| *saved == index) {
                    continue;
                }
                if let Some(uri) = frame.hyperlinks.get(index as usize) {
                    links.push((index, uri.clone()));
                }
            }
        }
        let style = Style::default()
            .fg(self.config.palette.text)
            .bg(self.config.palette.panel_bg);
        buffer.set_style(area, style);
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                buffer[(x, y)].set_symbol(" ");
            }
        }
        let status = match view.notice {
            Some(DraftNotice::UnsupportedControl) => "Text edits only",
            Some(DraftNotice::LimitReached) => "Scratchpad full",
            None => "Queued locally",
        };
        buffer.set_stringn(
            area.x,
            area.y,
            format!("Reconnect draft · {status}"),
            usize::from(area.width),
            style,
        );
        let cursor = text_editor::render(
            &mut buffer,
            Rect::new(area.x, area.y + 1, area.width, 1),
            view.editor,
            style.add_modifier(Modifier::UNDERLINED),
        );
        frame.replace_from_ratatui_buffer_preserving_effects(&buffer, cursor);
        self.reconnect_panel_underlay = Some(ReconnectPanelUnderlay {
            area,
            cells: underlay_cells,
            links,
        });
        occlusion.cover(area);
    }
}
