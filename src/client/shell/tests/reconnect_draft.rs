use super::prediction::{echo, remote_shell};
use super::*;
use crate::client::shell::reconnect_draft::{DraftReason, DraftTarget};

fn target() -> DraftTarget {
    DraftTarget {
        endpoint_id: ClientEndpointId::Local,
        pane_id: "pane_1".into(),
    }
}

fn editor() -> ClientShellState {
    let mut state = remote_shell(true, true);
    state.config.remote_buffer_reconnect_input = true;
    let mut projection = snapshot();
    projection.agents.push(ClientShellAgent {
        pane_id: "pane_1".into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: None,
        display_agent: Some("Codex".into()),
        agent: Some("codex".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Unknown,
        state_change_seq: 1,
        state_labels: vec![],
        tokens: vec![],
        focused: true,
    });
    state.set_snapshot(Box::new(projection));
    echo(&mut state, "› ");
    state.handle_input_bytes(b"a");
    echo(&mut state, "› a");
    state.active_snapshot_generation = Some(1);
    state.pane_surface_generation = Some(1);
    state.set_reconnect_input_ready(true);
    state
}

fn reconnect(state: &mut ClientShellState) {
    state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Online);
    state.active_snapshot_generation = Some(2);
    state.pane_surface_generation = Some(2);
    state.set_reconnect_input_ready(true);
}

#[test]
fn reconnect_draft_captures_and_edits_locally_through_presentation_sync() {
    let mut state = editor();
    let authoritative = state.pane_surface.clone().unwrap();
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    assert_eq!(
        state.reconnect_drafts.view(&target()).unwrap().reason,
        DraftReason::Ready
    );
    assert!(state
        .handle_input_bytes(b"mnop\x1b[D\x1b[D\x7f")
        .requests
        .is_empty());
    assert!(state.handle_input_bytes("é".as_bytes()).requests.is_empty());
    assert!(state
        .handle_input_bytes(b"\x1b[C\x1b[3~\x1b[Fz")
        .requests
        .is_empty());
    assert_eq!(state.reconnect_drafts.copy_text(&target()).unwrap(), "méoz");
    assert_eq!(state.pane_surface.as_ref().unwrap(), &authoritative);
    state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Online);
    assert!(
        state.take_reconnect_draft_input().is_none(),
        "Online is not committed input readiness"
    );
    assert!(state.handle_input_bytes(b"K").requests.is_empty());
    let frame = state.compose(80, 24).unwrap().frame;
    assert!(frame_rows(&frame)
        .iter()
        .any(|row| row.contains("Reconnect draft")));
    assert!(frame.cursor.as_ref().unwrap().visible);
    reconnect(&mut state);
    let request = state.take_reconnect_draft_input().unwrap();
    assert_eq!(
        request,
        ClientMessage::ClientShellPaneInput {
            pane_id: "pane_1".into(),
            events: vec![ClientPaneInputEvent::TextCommit("méozK".into())]
        }
    );
    assert!(state.take_reconnect_draft_input().is_none());
    echo(&mut state, "› améozK");
    assert!(state.reconnect_drafts.view(&target()).is_none());
    assert_eq!(state.handle_input_bytes(b"Q").requests.len(), 1);
}

#[test]
fn reconnect_draft_second_drop_keeps_attempted_and_new_text_without_retry() {
    let mut state = editor();
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    state.handle_input_bytes(b"one");
    reconnect(&mut state);
    state.take_reconnect_draft_input().unwrap();
    state.handle_input_bytes(b"two");
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    assert_eq!(
        state.reconnect_drafts.copy_text(&target()).unwrap(),
        "onetwo"
    );
    reconnect(&mut state);
    assert!(state.take_reconnect_draft_input().is_none());
    assert_eq!(
        state.reconnect_drafts.view(&target()).unwrap().reason,
        DraftReason::UncertainDelivery
    );
}

#[test]
fn reconnect_draft_changed_editor_and_uncertain_online_input_require_recovery() {
    for uncertain in [false, true] {
        let mut state = editor();
        if uncertain {
            state.handle_input_bytes(b"in-flight");
        }
        state.mark_endpoint_disconnected(&ClientEndpointId::Local);
        state.handle_input_bytes(b"offline");
        if !uncertain {
            echo(&mut state, "› changed");
        }
        reconnect(&mut state);
        assert!(state.take_reconnect_draft_input().is_none());
        assert_eq!(
            state.reconnect_drafts.copy_text(&target()).unwrap(),
            "offline"
        );
    }
}

#[test]
fn reconnect_draft_does_not_queue_enter_or_steal_prefix_and_keeps_copy_recovery() {
    let mut state = editor();
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    state.handle_input_bytes(b"offline\r");
    assert_eq!(
        state.reconnect_drafts.copy_text(&target()).unwrap(),
        "offline"
    );
    state.handle_input_bytes(b"\x02");
    assert_eq!(state.mode, ClientShellMode::Prefix);
    state.handle_input_bytes(b"\x1b");
    state.compose(80, 24).unwrap();
    let copy = state.hits.reconnect_copy;
    let copied =
        state.handle_raw_events(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: copy.x,
            row: copy.y,
            modifiers: KeyModifiers::NONE,
        })]);
    assert!(
        matches!(copied.actions.as_slice(),[ClientShellAction::ClipboardWrite(bytes)] if bytes == b"offline")
    );
    assert!(state.reconnect_drafts.view(&target()).is_some());
}

#[test]
fn reconnect_draft_replaced_pane_remains_copyable_without_sending_to_replacement() {
    let mut state = editor();
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    state.handle_input_bytes(b"recover-me");
    let mut replacement = snapshot();
    replacement.panes[0].pane_id = "replacement".into();
    replacement.focused_pane_id = Some("replacement".into());
    state.set_snapshot(Box::new(replacement));
    reconnect(&mut state);
    assert!(state.take_reconnect_draft_input().is_none());
    let frame = state.compose(80, 24).unwrap().frame;
    assert!(frame_rows(&frame)
        .iter()
        .any(|row| row.contains("copy to recover")));
    let copy = state.hits.reconnect_copy;
    let outcome =
        state.handle_raw_events(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: copy.x,
            row: copy.y,
            modifiers: KeyModifiers::NONE,
        })]);
    assert!(
        matches!(outcome.actions.as_slice(), [ClientShellAction::ClipboardWrite(text)] if text == b"recover-me")
    );
    assert!(outcome.requests.is_empty());
}

#[test]
fn reconnect_draft_panel_covers_remote_mouse_routing_and_retained_patches() {
    let mut state = editor();
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    state.handle_input_bytes(b"draft");
    reconnect(&mut state);
    state.reconnect_drafts.hold(&target());
    state.compose(80, 24).unwrap();
    let panel = state.hits.reconnect_panel;
    let clicked =
        state.handle_raw_events(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: panel.x,
            row: panel.y + 1,
            modifiers: KeyModifiers::NONE,
        })]);
    assert!(clicked.requests.is_empty());
    let surface = state.pane_surface.as_ref().unwrap();
    let mut pane = surface.panes[0].clone();
    pane.content_revision += 1;
    let patch = crate::protocol::PaneSurfacePatch {
        boot_id: surface.boot_id.clone(),
        projection_revision: surface.projection_revision,
        base_surface_revision: surface.surface_revision,
        surface_revision: surface.surface_revision + 1,
        rows: vec![crate::protocol::PaneSurfacePatchRow {
            x: 0,
            y: 1,
            cells: vec![surface.frame.cells[0].clone()],
        }],
        panes: vec![pane],
        cursor: surface.frame.cursor.clone(),
    };
    assert!(matches!(
        state.apply_pane_surface_patch(patch),
        ClientPaneSurfacePatchOutcome::Applied(None)
    ));
    let frame = state.compose(80, 24).unwrap().frame;
    assert!(frame_rows(&frame)
        .iter()
        .any(|row| row.contains("Reconnect draft")));
}

#[test]
#[ignore = "supporting render scaling profile; run manually in release mode"]
fn reconnect_draft_render_scale_profile() {
    use std::{hint::black_box, time::Instant};
    let mut medians = Vec::new();
    for panes in [1, 15] {
        let mut samples = Vec::new();
        for _ in 0..9 {
            let mut state = super::prediction::populated_remote_prediction_shell(panes, true);
            state.config.remote_buffer_reconnect_input = true;
            state.mark_endpoint_disconnected(&ClientEndpointId::Local);
            state.handle_input_bytes("draft éλ text".as_bytes());
            for _ in 0..20 {
                black_box(state.compose(120, 48).unwrap());
            }
            let start = Instant::now();
            for _ in 0..64 {
                black_box(state.compose(120, 48).unwrap());
            }
            samples.push(start.elapsed().as_secs_f64() * 1_000_000.0 / 64.0);
        }
        samples.sort_by(f64::total_cmp);
        medians.push(samples[4]);
        println!(
            "reconnect draft 120x48, {panes} populated panes: {:.2} us/render (9x64, 20 warmups)",
            samples[4]
        );
    }
    println!(
        "reconnect draft 1 -> 15 pane scaling: {:.2}x (+{:.2} us/render)",
        medians[1] / medians[0],
        medians[1] - medians[0]
    );
}

#[test]
fn reconnect_draft_projects_only_local_edits_over_last_presented_frozen_surface() {
    let mut state = editor();
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    state.handle_input_bytes(b"before");
    let source = state.compose(80, 24).unwrap().frame;
    let previous_panel = state.hits.reconnect_panel;
    state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Online);
    state.pane_surface_generation = Some(2);
    assert!(
        state.compose(80, 24).is_none(),
        "candidate lacks matching projection generation"
    );
    state.handle_input_bytes("éλ".as_bytes());
    let mut frozen_source = source.clone();
    frozen_source.graphics = b"unreplayed remote graphics".to_vec();
    let projected = state.project_reconnect_draft(&frozen_source).unwrap();
    assert!(projected.graphics.is_empty());
    assert!(frame_rows(&projected)
        .iter()
        .any(|row| row.contains("beforeéλ")));
    assert!(projected.cursor.as_ref().unwrap().visible);
    for y in 0..source.height {
        for x in 0..source.width {
            if !contains(previous_panel, (x, y)) {
                let index = usize::from(y) * usize::from(source.width) + usize::from(x);
                assert_eq!(projected.cells[index], source.cells[index]);
            }
        }
    }
    assert!(state.take_reconnect_draft_input().is_none());
}

#[test]
fn reconnect_draft_frozen_repaints_restore_underlay_on_hide_move_and_discard() {
    let mut client = crate::client::ClientState::test_new();
    client.shell = Some(editor());
    let shell = client.shell.as_mut().unwrap();
    shell.mark_endpoint_disconnected(&ClientEndpointId::Local);
    shell.handle_input_bytes(b"first");
    let frame = shell.compose(80, 24).unwrap().frame;
    let old_panel = shell.hits.reconnect_panel;
    let encoded = client.blit_encoder.encode(&frame, true);
    client.presented_reconnect_underlay = client.shell.as_mut().unwrap().take_reconnect_underlay();
    client.blit_encoder.commit(frame, encoded);
    client.freeze_presentation();
    let clean = client.frozen_reconnect_base.clone().unwrap();
    assert!(!frame_rows(&clean)
        .iter()
        .any(|row| row.contains("Reconnect draft")));
    for text in ["é", "λ"] {
        assert!(client
            .shell
            .as_mut()
            .unwrap()
            .handle_input_bytes(text.as_bytes())
            .requests
            .is_empty());
        let projected = client.frozen_reconnect_draft_frame().unwrap();
        assert!(frame_rows(&projected)
            .iter()
            .any(|row| row.contains("first")));
        let encoded = client.blit_encoder.encode(&projected, false);
        client.blit_encoder.commit(projected, encoded);
    }
    assert!(frame_rows(client.blit_encoder.current_frame().unwrap())
        .iter()
        .any(|row| row.contains("firstéλ")));
    client.shell.as_mut().unwrap().mode = ClientShellMode::Prefix;
    assert_eq!(client.frozen_reconnect_draft_frame().unwrap(), clean);
    assert!(client
        .shell
        .as_ref()
        .unwrap()
        .hits
        .reconnect_copy
        .is_empty());
    client.shell.as_mut().unwrap().mode = ClientShellMode::Terminal;
    let shell = client.shell.as_mut().unwrap();
    let surface = shell.pane_surface.as_mut().unwrap();
    surface.panes[0].inner_rect.y = 1;
    surface.panes[0].inner_rect.height = 5;
    let moved = client.frozen_reconnect_draft_frame().unwrap();
    let new_panel = client.shell.as_ref().unwrap().hits.reconnect_panel;
    assert_ne!(old_panel, new_panel);
    for y in old_panel.y..old_panel.bottom() {
        for x in old_panel.x..old_panel.right() {
            let index = usize::from(y) * usize::from(clean.width) + usize::from(x);
            assert_eq!(moved.cells[index], clean.cells[index]);
        }
    }
    client
        .shell
        .as_mut()
        .unwrap()
        .reconnect_drafts
        .discard(&target());
    let restored = client.frozen_reconnect_draft_frame().unwrap();
    assert_eq!(restored, clean);
    let hits = &client.shell.as_ref().unwrap().hits;
    assert!(
        hits.reconnect_panel.is_empty()
            && hits.reconnect_copy.is_empty()
            && hits.reconnect_discard.is_empty()
    );
    assert!(client.presentation_frozen);
    client.unfreeze_presentation();
    assert!(client.frozen_reconnect_base.is_none());
}

#[test]
fn reconnect_draft_first_appearing_during_frozen_handoff_gets_local_projection() {
    let mut client = crate::client::ClientState::test_new();
    client.shell = Some(editor());
    let frame = client
        .shell
        .as_mut()
        .unwrap()
        .compose(80, 24)
        .unwrap()
        .frame;
    let encoded = client.blit_encoder.encode(&frame, true);
    client.blit_encoder.commit(frame, encoded);
    client.freeze_presentation();
    assert!(client.frozen_reconnect_base.is_none());
    client
        .shell
        .as_mut()
        .unwrap()
        .mark_endpoint_disconnected(&ClientEndpointId::Local);
    client
        .shell
        .as_mut()
        .unwrap()
        .handle_input_bytes(b"late-draft");
    let projected = client.frozen_reconnect_draft_frame().unwrap();
    assert!(frame_rows(&projected)
        .iter()
        .any(|row| row.contains("late-draft")));
    assert!(client.presentation_frozen);
}

#[test]
fn reconnect_draft_underlay_restores_original_link_targets_after_table_remapping() {
    let mut state = editor();
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    state.handle_input_bytes(b"draft");
    let source = state.compose(80, 24).unwrap().frame;
    let panel = state.hits.reconnect_panel;
    let covered = usize::from(panel.y) * usize::from(source.width) + usize::from(panel.x);
    let surviving = 0;
    let mut base = state.take_reconnect_underlay().unwrap().clean_base(&source);
    base.hyperlinks = vec![
        "https://example.test/covered".into(),
        "https://example.test/surviving".into(),
    ];
    base.cells[covered].hyperlink = Some(0);
    base.cells[surviving].hyperlink = Some(1);
    let painted = state.project_reconnect_draft(&base).unwrap();
    assert_eq!(painted.hyperlinks, vec!["https://example.test/surviving"]);
    let restored = state
        .take_reconnect_underlay()
        .unwrap()
        .clean_base(&painted);
    assert_eq!(
        restored.hyperlinks[restored.cells[covered].hyperlink.unwrap() as usize],
        "https://example.test/covered"
    );
    assert_eq!(
        restored.hyperlinks[restored.cells[surviving].hyperlink.unwrap() as usize],
        "https://example.test/surviving"
    );
}
