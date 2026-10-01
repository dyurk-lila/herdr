use super::prediction::{echo, remote_shell};
use super::*;
use crate::client::shell::reconnect_draft::DraftTarget;

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
fn reconnect_draft_panel_covers_remote_mouse_routing_and_retained_patches() {
    let mut state = editor();
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    state.handle_input_bytes(b"draft");
    reconnect(&mut state);
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
fn reconnect_draft_frozen_repaints_restore_underlay_on_hide_move_and_delivery() {
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
        .reconnect_panel
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
    assert!(hits.reconnect_panel.is_empty());
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

#[test]
fn scratchpad_waits_for_committed_readiness_then_flushes_all_edited_text() {
    let mut state = editor();
    let authoritative = state.pane_surface.clone().unwrap();
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    assert!(state
        .handle_input_bytes(b"mnop\x1b[D\x1b[D\x7f")
        .requests
        .is_empty());
    state.handle_input_bytes("é".as_bytes());
    state.handle_input_bytes(b"\x1b[C\x1b[3~\x1b[Fz");
    assert_eq!(state.reconnect_drafts.copy_text(&target()).unwrap(), "méoz");
    assert_eq!(state.pane_surface.as_ref().unwrap(), &authoritative);
    state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Online);
    assert!(state.take_reconnect_draft_input().is_none());
    assert!(state.handle_input_bytes(b"K").requests.is_empty());
    reconnect(&mut state);
    let input = state.take_reconnect_draft_input().unwrap();
    assert_eq!(
        input.request,
        ClientMessage::ClientShellPaneInput {
            pane_id: target().pane_id,
            events: vec![ClientPaneInputEvent::TextCommit("méozK".into())]
        }
    );
    assert!(
        state.has_reconnect_draft(),
        "prepare does not imply acceptance"
    );
    state.commit_reconnect_draft_handoff(&input);
    assert!(state.reconnect_drafts.view(&target()).is_none());
    let frame = state.compose(80, 24).unwrap().frame;
    assert!(!frame_rows(&frame)
        .iter()
        .any(|row| row.contains("Reconnect draft")));
    assert!(state.hits.reconnect_panel.is_empty());
    assert_eq!(state.handle_input_bytes(b"Q").requests.len(), 1);
}

#[test]
fn scratchpad_always_transfers_with_unconfirmed_changed_or_unrecognized_editor() {
    for variant in 0..4 {
        let mut state = editor();
        match variant {
            0 => {
                state.handle_input_bytes(b"in-flight");
            }
            1 => {
                state.handle_input_bytes(b"\x02\x1b");
            }
            2 => {
                state.config.remote_predict_input = false;
            }
            _ => {}
        }
        state.mark_endpoint_disconnected(&ClientEndpointId::Local);
        state.handle_input_bytes("界🧪λoffline".as_bytes());
        if variant == 3 {
            let mut projection = snapshot();
            projection.agents.clear();
            state.set_snapshot(Box::new(projection));
            echo(&mut state, "totally different editor");
        } else {
            echo(&mut state, "› changed");
        }
        reconnect(&mut state);
        let input = state.take_reconnect_draft_input().unwrap();
        assert!(
            matches!(&input.request, ClientMessage::ClientShellPaneInput { events, .. }
            if events == &vec![ClientPaneInputEvent::TextCommit("界🧪λoffline".into())])
        );
        state.commit_reconnect_draft_handoff(&input);
        assert!(!state.has_reconnect_draft());
    }
}

#[test]
fn scratchpad_accepted_text_never_returns_on_later_loss_without_echo() {
    let mut state = editor();
    for text in ["界first", "λsecond", "third"] {
        state.mark_endpoint_disconnected(&ClientEndpointId::Local);
        assert_eq!(state.reconnect_drafts.copy_text(&target()).unwrap(), "");
        state.handle_input_bytes(text.as_bytes());
        reconnect(&mut state);
        let input = state.take_reconnect_draft_input().unwrap();
        assert!(
            matches!(&input.request, ClientMessage::ClientShellPaneInput {events,..}
            if events == &vec![ClientPaneInputEvent::TextCommit(text.into())])
        );
        assert!(state.commit_reconnect_draft_handoff(&input));
        assert!(state.take_reconnect_draft_input().is_none());
    }
}

#[test]
fn scratchpad_failed_enqueue_retries_and_clears_only_after_success() {
    use crate::client::endpoint::{EndpointNegotiation, EndpointRegistry, EndpointTransport};
    struct Transport(bool);
    impl EndpointTransport for Transport {
        fn send(&mut self, _: &ClientMessage) -> std::io::Result<()> {
            if self.0 {
                Ok(())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "test enqueue failed",
                ))
            }
        }
    }
    let mut state = editor();
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    state.handle_input_bytes(b"offline");
    reconnect(&mut state);
    for success in [false, true] {
        let input = state.take_reconnect_draft_input().unwrap();
        let mut endpoints = EndpointRegistry::new(
            Transport(success),
            1,
            EndpointNegotiation::new(Vec::new(), Vec::new()),
        );
        assert_eq!(
            crate::client::shell_runtime::send_reconnect_draft(&mut endpoints, &mut state, &input),
            success
        );
        assert_eq!(state.has_reconnect_draft(), !success);
        if !success {
            state.mark_endpoint_disconnected(&ClientEndpointId::Local);
            reconnect(&mut state);
            assert_eq!(
                state.reconnect_drafts.copy_text(&target()).unwrap(),
                "offline"
            );
        }
    }
    assert!(state.take_reconnect_draft_input().is_none());
}

#[test]
fn scratchpad_empty_editor_removal_repaints_the_presented_frame() {
    use crate::client::endpoint::{EndpointNegotiation, EndpointRegistry, EndpointTransport};
    struct Transport;
    impl EndpointTransport for Transport {
        fn send(&mut self, _: &ClientMessage) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut shell = editor();
    shell.mark_endpoint_disconnected(&ClientEndpointId::Local);
    shell.handle_input_bytes(b"x");
    reconnect(&mut shell);
    let outcome = shell.handle_input_bytes(b"\x7f");
    let frame = shell.compose(80, 24).unwrap();
    assert!(frame_rows(&frame.frame)
        .iter()
        .any(|row| row.contains("Reconnect draft")));
    let mut state = crate::client::ClientState::test_new();
    state.reported_size = (80, 24);
    state.shell = Some(shell);
    // The loop-top readiness refresh must leave removal to the runtime repaint path.
    state
        .shell
        .as_mut()
        .unwrap()
        .set_reconnect_input_ready(true);
    assert!(state.shell.as_ref().unwrap().has_reconnect_draft());
    let mut endpoints = EndpointRegistry::new(
        Transport,
        1,
        EndpointNegotiation::new(Vec::new(), Vec::new()),
    );
    crate::client::shell_runtime::finish_client_shell_input(
        &mut state,
        outcome,
        Some(frame),
        &mut endpoints,
        &mut None,
        &mut crate::client::endpoint_commands::EndpointCommands::default(),
        &mut crate::platform::RealPrefixInputSource::default(),
        &mut None,
    )
    .unwrap();
    assert!(!state.shell.as_ref().unwrap().has_reconnect_draft());
    assert!(!frame_rows(state.blit_encoder.current_frame().unwrap())
        .iter()
        .any(|row| row.contains("Reconnect draft")));
}

#[test]
fn scratchpad_waits_for_popup_pane_input_to_be_available_then_transfers() {
    for popup_kind in 0..3 {
        let mut state = editor();
        state.mark_endpoint_disconnected(&ClientEndpointId::Local);
        state.handle_input_bytes(b"offline");
        reconnect(&mut state);
        match popup_kind {
            0 => state.popup_terminal_id = Some("popup".into()),
            1 => state.popup_pending = true,
            _ => {
                state.pane_surface.as_mut().unwrap().popup =
                    Some(Box::new(crate::protocol::ClientShellPopupSurface {
                        terminal_id: "popup".into(),
                        title: String::new(),
                        width: None,
                        height: None,
                        frame: state.pane_surface.as_ref().unwrap().frame.clone(),
                        mouse_reporting: false,
                        sgr_pixel_mouse: false,
                        pixel_width: 0,
                        pixel_height: 0,
                    }))
            }
        }
        assert!(state.take_reconnect_draft_input().is_none());
        assert_eq!(
            state.reconnect_drafts.copy_text(&target()).unwrap(),
            "offline"
        );
        state.popup_terminal_id = None;
        state.popup_pending = false;
        state.pane_surface.as_mut().unwrap().popup = None;
        let input = state.take_reconnect_draft_input().unwrap();
        assert!(
            matches!(&input.request, ClientMessage::ClientShellPaneInput { events, .. }
            if events == &vec![ClientPaneInputEvent::TextCommit("offline".into())])
        );
        assert!(state.commit_reconnect_draft_handoff(&input));
        assert!(!state.has_reconnect_draft());
    }
}

#[test]
fn scratchpad_has_no_recovery_buttons_and_never_queues_enter() {
    let mut state = editor();
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    state.handle_input_bytes(b"offline\r");
    let frame = state.compose(80, 24).unwrap().frame;
    let rows = frame_rows(&frame);
    assert!(rows.iter().any(|row| row.contains("Reconnect draft")));
    assert!(!rows.iter().any(|row| row.contains("[Copy]")
        || row.contains("[Discard]")
        || row.contains("copy to recover")));
    state.handle_input_bytes(b"\x02");
    assert_eq!(state.mode, ClientShellMode::Prefix);
    state.handle_input_bytes(b"\x1b");
    reconnect(&mut state);
    let input = state.take_reconnect_draft_input().unwrap();
    assert!(
        matches!(input.request, ClientMessage::ClientShellPaneInput {events,..}
        if events == vec![ClientPaneInputEvent::TextCommit("offline".into())])
    );
}

#[test]
fn scratchpad_toggle_off_stops_new_capture_without_stranding_existing_text() {
    let mut state = editor();
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    state.handle_input_bytes(b"offline");
    state.config.remote_buffer_reconnect_input = false;
    assert!(state.handle_input_bytes(b"K").requests.is_empty());
    reconnect(&mut state);
    let input = state.take_reconnect_draft_input().unwrap();
    assert!(
        matches!(&input.request, ClientMessage::ClientShellPaneInput {events,..}
        if events == &vec![ClientPaneInputEvent::TextCommit("offlineK".into())])
    );
    state.commit_reconnect_draft_handoff(&input);
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    assert!(!state.has_reconnect_draft());
}

#[test]
fn scratchpad_retargets_missing_or_nonviewed_pane_on_same_endpoint_and_removes_source() {
    for removed in [true, false] {
        let mut state = editor();
        state.mark_endpoint_disconnected(&ClientEndpointId::Local);
        state.handle_input_bytes(b"offline");
        let mut projection = snapshot();
        let mut replacement = projection.panes[0].clone();
        replacement.pane_id = "replacement".into();
        if removed {
            projection.panes.clear();
        }
        projection.panes.push(replacement);
        projection.focused_pane_id = Some("replacement".into());
        state.set_snapshot(Box::new(projection));
        let mut visible = state.pane_surface.clone().unwrap();
        visible.panes[0].pane_id = "replacement".into();
        state.set_pane_surface(visible);
        reconnect(&mut state);
        let input = state.take_reconnect_draft_input().unwrap();
        assert!(
            matches!(&input.request, ClientMessage::ClientShellPaneInput {pane_id,..} if pane_id == "replacement")
        );
        assert!(state.commit_reconnect_draft_handoff(&input));
        assert!(state.reconnect_drafts.view(&target()).is_none());
        assert!(!state.has_reconnect_draft());
    }
}

#[test]
fn scratchpad_stays_with_original_machine_and_multiple_entries_drain() {
    let mut state = editor();
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    state.handle_input_bytes(b"first");
    let second = DraftTarget {
        endpoint_id: ClientEndpointId::Local,
        pane_id: "other-tab-pane".into(),
    };
    state.reconnect_drafts.begin(&second);
    state.reconnect_drafts.edit(
        &second,
        &crate::raw_input::RawInputEvent::Paste("second".into()),
    );
    let other = ClientEndpointId::Ssh(
        crate::client::endpoint::ProfileId::parse("11111111111111111111111111111111").unwrap(),
    );
    state.active_endpoint_id = other.clone();
    state.set_endpoint_status(&other, ClientEndpointStatus::Online);
    state.set_reconnect_input_ready(true);
    assert!(state.take_reconnect_draft_input().is_none());
    state.active_endpoint_id = ClientEndpointId::Local;
    reconnect(&mut state);
    for text in ["first", "second"] {
        let input = state.take_reconnect_draft_input().unwrap();
        assert!(
            matches!(&input.request, ClientMessage::ClientShellPaneInput {events,..}
            if events == &vec![ClientPaneInputEvent::TextCommit(text.into())])
        );
        state.commit_reconnect_draft_handoff(&input);
    }
    assert!(state.take_reconnect_draft_input().is_none());
}

#[test]
fn scratchpad_middle_cursor_is_restored_within_server_batch_limit() {
    let mut state = editor();
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    state.handle_input_bytes(b"abc\x1b[D");
    reconnect(&mut state);
    let input = state.take_reconnect_draft_input().unwrap();
    let ClientMessage::ClientShellPaneInput { events, .. } = input.request else {
        panic!("pane input")
    };
    assert!(matches!(&events[0], ClientPaneInputEvent::TextCommit(text) if text == "abc"));
    assert!(matches!(
        &events[1],
        ClientPaneInputEvent::Key {
            code: crate::protocol::ClientKeyCode::Left,
            repeat_count: 1,
            ..
        }
    ));
}

#[test]
fn scratchpad_full_budget_home_preserves_text_without_exceeding_server_event_limit() {
    let mut state = editor();
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    state.handle_raw_events(vec![crate::raw_input::RawInputEvent::Paste(
        "x".repeat(64 * 1024),
    )]);
    state.handle_input_bytes(b"\x1b[H");
    reconnect(&mut state);
    let input = state.take_reconnect_draft_input().unwrap();
    let ClientMessage::ClientShellPaneInput { events, .. } = &input.request else {
        panic!("pane input")
    };
    assert!(
        matches!(&events[0], ClientPaneInputEvent::TextCommit(text) if text.len() == 64 * 1024)
    );
    assert_eq!(
        events.len(),
        1,
        "huge cursor movement must not reject the entire text batch"
    );
    assert!(state.commit_reconnect_draft_handoff(&input));
    assert!(!state.has_reconnect_draft());
}
