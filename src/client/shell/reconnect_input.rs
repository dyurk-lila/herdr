//! Client-owned outage drafts; remote input leases and authoritative frames stay unchanged.
use super::reconnect_draft::{DraftNotice, DraftReason, DraftTarget};
use super::*;
use crate::raw_input::RawInputEvent;
use crossterm::event::{KeyEventKind, MouseButton, MouseEventKind};
use std::time::Instant;

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
            .recovery_target(&self.active_endpoint_id, |pane_id| {
                self.snapshot.as_ref().is_some_and(|snapshot| {
                    snapshot.panes.iter().any(|pane| pane.pane_id == pane_id)
                })
            })
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

    fn reconnect_anchor(
        &self,
        target: &DraftTarget,
    ) -> Option<(reconnect_draft::DraftAnchor, bool)> {
        self.input_prediction.reconnect_anchor(
            self.pane_surface.as_ref()?,
            &target.pane_id,
            self.reconnect_agent(&target.pane_id)?,
        )
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
        let anchor = self.reconnect_anchor(&target);
        let allow_auto = anchor.as_ref().is_some_and(|(_, confirmed)| *confirmed);
        self.reconnect_drafts.disconnect(&target);
        if !self
            .reconnect_drafts
            .begin(&target, anchor.map(|(anchor, _)| anchor), allow_auto)
        {
            self.set_endpoint_error("Reconnect draft limit reached; discard an older draft");
        }
        self.reconnect_input_ready = false;
    }

    pub(crate) fn set_reconnect_input_ready(&mut self, ready: bool) {
        self.reconnect_input_ready = ready;
        if ready {
            if let Some(target) = self.reconnect_target() {
                if self.reconnect_drafts.view(&target).is_some_and(|view| {
                    view.editor.is_empty() && view.attempted.is_none() && view.uncertain.is_none()
                }) {
                    self.reconnect_drafts.discard(&target);
                }
            }
            if let Some(target) = self.reconnect_display_target() {
                if self.reconnect_target().as_ref() != Some(&target) {
                    self.reconnect_drafts.hold(&target);
                }
            }
            self.observe_reconnect_draft();
        }
    }

    pub(crate) fn block_reconnect_draft_recovery(&mut self) {
        if let Some(target) = self.reconnect_target() {
            self.reconnect_drafts.hold(&target);
        }
    }

    pub(super) fn observe_reconnect_draft(&mut self) {
        if !self.reconnect_input_ready
            || self.pending_pane_surface.is_some()
            || self.pane_surface_generation != self.active_snapshot_generation
        {
            return;
        }
        let (Some(target), Some(generation)) =
            (self.reconnect_target(), self.active_snapshot_generation)
        else {
            return;
        };
        if let Some((anchor, _)) = self.reconnect_anchor(&target) {
            let awaiting_echo = self
                .reconnect_drafts
                .view(&target)
                .is_some_and(|view| view.reason == DraftReason::AwaitingEcho);
            let changed =
                self.reconnect_drafts
                    .observe(&target, &anchor, generation, Instant::now());
            let confirmed = awaiting_echo
                && changed
                && self
                    .reconnect_drafts
                    .view(&target)
                    .is_none_or(|view| view.reason == DraftReason::Ready);
            if confirmed {
                self.input_prediction
                    .adopt_reconnect_echo(&target.pane_id, &anchor);
            }
        }
    }

    pub(crate) fn take_reconnect_draft_input(&mut self) -> Option<ClientMessage> {
        if !self.config.remote_buffer_reconnect_input
            || !self.reconnect_input_ready
            || self.pending_pane_surface.is_some()
            || self.pane_surface_generation != self.active_snapshot_generation
            || self.overlay.is_some()
            || self.mode != ClientShellMode::Terminal
        {
            return None;
        }
        let target = self.reconnect_target()?;
        let (anchor, _) = self.reconnect_anchor(&target)?;
        let generation = self.active_snapshot_generation?;
        let attempt =
            self.reconnect_drafts
                .attempt(&target, &anchor, generation, Instant::now())?;
        let mut events = vec![ClientPaneInputEvent::TextCommit(attempt.text)];
        if attempt.trailing_left > 0 {
            events.push(ClientPaneInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Left,
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,
                repeat_count: attempt.trailing_left as u16,
                generated_text: None,
                shifted_codepoint: None,
                tracks_release: false,
                physical_key_id: None,
                windows_record: None,
            });
        }
        Some(ClientMessage::ClientShellPaneInput {
            pane_id: target.pane_id,
            events,
        })
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
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) && mouse.modifiers.is_empty() {
                let point = (mouse.column, mouse.row);
                if contains(self.hits.reconnect_copy, point) {
                    if let Some(text) = self.reconnect_drafts.copy_text(&target) {
                        outcome
                            .actions
                            .push(ClientShellAction::ClipboardWrite(text.into_bytes()));
                    }
                    outcome.repaint = true;
                    return true;
                }
                if contains(self.hits.reconnect_discard, point) {
                    outcome.repaint |= self.reconnect_drafts.discard(&target);
                    return true;
                }
            }
            if contains(self.hits.reconnect_panel, (mouse.column, mouse.row)) {
                return true;
            }
        }
        if !self.config.remote_buffer_reconnect_input
            || self.mode != ClientShellMode::Terminal
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
        self.hits.reconnect_copy = Rect::default();
        self.hits.reconnect_discard = Rect::default();
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
            Some(DraftNotice::UnsupportedControl) => "edit text only; copy to recover",
            Some(DraftNotice::LimitReached) => "Draft full; copy to recover",
            None => match view.reason {
                DraftReason::Ready => "Queued locally",
                DraftReason::AwaitingEcho => "Waiting for remote echo",
                DraftReason::UncertainDelivery => "Delivery uncertain; copy to recover",
                _ => "Editor changed; copy to recover",
            },
        };
        let label = if self.reconnect_target().as_ref() == Some(&target) {
            format!("Reconnect draft · {status}")
        } else {
            format!("Draft · copy to recover ({})", target.pane_id)
        };
        buffer.set_stringn(
            area.x,
            area.y,
            label,
            usize::from(area.width.saturating_sub(17)),
            style,
        );
        self.hits.reconnect_copy = Rect::new(area.right() - 16, area.y, 6, 1);
        self.hits.reconnect_discard = Rect::new(area.right() - 9, area.y, 9, 1);
        buffer.set_string(
            self.hits.reconnect_copy.x,
            area.y,
            "[Copy]",
            style.fg(self.config.palette.blue),
        );
        buffer.set_string(
            self.hits.reconnect_discard.x,
            area.y,
            "[Discard]",
            style.fg(self.config.palette.blue),
        );
        let prefix = view.uncertain.or(view.attempted).unwrap_or("");
        let prefix_width = UnicodeWidthStr::width(prefix).min(usize::from(area.width / 2)) as u16;
        buffer.set_stringn(area.x, area.y + 1, prefix, usize::from(prefix_width), style);
        let cursor = text_editor::render(
            &mut buffer,
            Rect::new(
                area.x + prefix_width,
                area.y + 1,
                area.width - prefix_width,
                1,
            ),
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
