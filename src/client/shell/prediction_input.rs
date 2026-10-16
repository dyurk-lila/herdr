use super::*;
use crate::raw_input::RawInputEvent;
use crossterm::event::{KeyEventKind, KeyModifiers};
use std::time::Instant;

impl ClientShellState {
    pub(crate) fn prediction_enabled(&self) -> bool {
        self.config.remote_predict_input
    }
    pub(crate) fn initialize_prediction_profiles(
        &mut self,
        profiles: super::prediction_profiles::Profiles,
    ) -> bool {
        self.input_prediction.set_profiles(profiles)
    }

    pub(crate) fn pause_prediction_profiles_until_epoch(&mut self, epoch: u64) -> bool {
        self.input_prediction.pause_profiles_until_epoch(epoch)
    }

    pub(crate) fn take_prediction_profile_updates(
        &mut self,
    ) -> Vec<super::prediction_profiles::ProfileUpdate> {
        self.input_prediction.take_profile_updates()
    }

    pub(crate) fn set_primary_prediction_target(&mut self, target: &str) {
        self.input_prediction
            .set_machine_target(ClientEndpointId::Local, target);
    }
    fn prediction_agent_context(&self) -> Option<crate::detect::Agent> {
        let pane_id = self.focused_pane_id()?;
        let agent = self
            .snapshot
            .as_ref()?
            .agents
            .iter()
            .find(|agent| agent.pane_id == pane_id)?;
        crate::detect::parse_agent_label(agent.agent.as_deref()?)
    }
    pub(super) fn prediction_allowed(&self) -> bool {
        self.prediction_context_allowed()
            && self.pending_pane_surface.is_none()
            && self.pane_surface_generation == self.active_snapshot_generation
    }

    pub(super) fn prediction_context_allowed(&self) -> bool {
        self.config.remote_predict_input
            && (self.primary_remote || !self.active_endpoint_id.is_local())
            && self.endpoint_is_online(&self.active_endpoint_id)
            && self.mode == ClientShellMode::Terminal
            && self.overlay.is_none()
            && self.selection.is_none()
            && self.copy_mode.is_none()
            && !self.popup_pending
            && self.popup_terminal_id.is_none()
            && self.endpoint_error.is_none()
            && self.outer_focused != Some(false)
    }

    pub(super) fn prepare_prediction_input(
        &mut self,
        event: &RawInputEvent,
        outcome: &mut ClientShellInput,
    ) {
        if matches!(event, RawInputEvent::Key(key) if key.code == KeyCode::Enter && key.modifiers.is_empty() && key.kind == KeyEventKind::Press)
            && self.prediction_allowed()
        {
            let agent = self.prediction_agent_context();
            outcome.repaint |= self
                .input_prediction
                .select_machine(&self.active_endpoint_id);
            outcome.repaint |= self.input_prediction.set_agent_context(agent);
            outcome.repaint |= self.input_prediction.submit();
            return;
        }
        // Reset before routing controls, even when the client consumes them. Printable
        // shortcuts are never predicted unless they actually reach a remote pane below.
        let simple = match event {
            RawInputEvent::Key(key) => {
                key.kind == KeyEventKind::Release
                    || (matches!(key.code, KeyCode::Char(c) if !c.is_control())
                        && (key.modifiers - KeyModifiers::SHIFT).is_empty())
                    || (matches!(
                        key.code,
                        KeyCode::Backspace
                            | KeyCode::Delete
                            | KeyCode::Left
                            | KeyCode::Right
                            | KeyCode::End
                    ) && key.modifiers.is_empty())
                    || (key.code == KeyCode::Char('w') && key.modifiers == KeyModifiers::CONTROL)
                    || (key.code == KeyCode::Backspace && key.modifiers == KeyModifiers::ALT)
            }
            RawInputEvent::Mouse(mouse) => {
                mouse.modifiers.is_empty()
                    && matches!(
                        mouse.kind,
                        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left)
                            | crossterm::event::MouseEventKind::Up(
                                crossterm::event::MouseButton::Left
                            )
                            | crossterm::event::MouseEventKind::Moved
                    )
            }
            RawInputEvent::Text(_) => true,
            RawInputEvent::HostDefaultColor { .. }
            | RawInputEvent::HostPaletteColors { .. }
            | RawInputEvent::HostCellSizeReport { .. }
            | RawInputEvent::HostColorSchemeChanged(_) => true,
            _ => false,
        };
        if !simple || !self.prediction_allowed() {
            outcome.repaint |= self.input_prediction.clear();
        }
    }

    pub(super) fn predict_pane_event(
        &mut self,
        target: &ClientInputTarget,
        event: &ClientPaneInputEvent,
        outcome: &mut ClientShellInput,
    ) {
        if !self.prediction_allowed() {
            outcome.repaint |= self.input_prediction.clear();
            return;
        }
        if let (ClientInputTarget::Pane(pane_id), Some(surface)) =
            (target, self.pane_surface.as_ref())
        {
            let agent = self.prediction_agent_context();
            outcome.repaint |= self
                .input_prediction
                .select_machine(&self.active_endpoint_id);
            outcome.repaint |= self.input_prediction.set_agent_context(agent);
            outcome.repaint |=
                self.input_prediction
                    .record_input(surface, pane_id, event, Instant::now());
        }
    }

    pub(super) fn reconcile_prediction(&mut self) {
        if !self.prediction_allowed() {
            self.input_prediction.clear();
        } else if let Some(surface) = self.pane_surface.as_ref() {
            let agent = self.prediction_agent_context();
            self.input_prediction
                .select_machine(&self.active_endpoint_id);
            self.input_prediction.set_agent_context(agent);
            self.input_prediction.observe(surface, Instant::now());
        }
    }

    pub(crate) fn tick_prediction(&mut self, now: Instant) -> bool {
        if !self.prediction_context_allowed() {
            self.input_prediction.clear()
        } else {
            self.input_prediction.expire(now)
        }
    }
}
