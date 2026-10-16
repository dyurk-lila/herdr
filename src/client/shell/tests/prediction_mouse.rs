use super::*;

fn remote_mouse_shell(reporting: bool) -> ClientShellState {
    let mut config = Config::default();
    config.remote.predict_input = true;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.primary_remote = true;
    state.set_snapshot(Box::new(snapshot()));
    let size = state.surface_size(80, 24);
    let mut initial = surface();
    let rect = SurfaceRect {
        x: 0,
        y: 0,
        width: size.cols,
        height: size.rows,
    };
    initial.panes[0].rect = rect;
    initial.panes[0].inner_rect = rect;
    initial.panes[0].mouse_reporting = reporting;
    initial.frame = FrameData::from_ratatui_buffer(
        &Buffer::empty(Rect::new(0, 0, size.cols, size.rows)),
        Some(crate::protocol::CursorState {
            x: 0,
            y: 0,
            visible: true,
            shape: 2,
        }),
    );
    state.set_pane_surface(initial);
    state.compose(80, 24).expect("initial frame");
    state.handle_input_bytes(b"a");
    echo_at(&mut state, "a", 1);
    state.handle_input_bytes(b"b");
    echo_at(&mut state, "ab", 2);
    state.compose(80, 24).expect("trained frame");
    state
}

fn echo_at(state: &mut ClientShellState, text: &str, cursor: u16) {
    let mut next = state.pane_surface.clone().expect("authoritative surface");
    for cell in &mut next.frame.cells[..usize::from(next.frame.width)] {
        cell.symbol = " ".into();
    }
    for (column, ch) in text.chars().enumerate() {
        next.frame.cells[column].symbol = ch.to_string();
    }
    next.frame.cursor.as_mut().expect("cursor").x = cursor;
    next.surface_revision += 1;
    next.panes[0].content_revision += 1;
    state.set_pane_surface(next);
}

fn mouse_at(
    state: &ClientShellState,
    kind: MouseEventKind,
    column: u16,
    modifiers: KeyModifiers,
) -> MouseEvent {
    let pane = &state.hits.panes[0];
    MouseEvent {
        kind,
        column: pane.inner_rect.x + column,
        row: pane.inner_rect.y,
        modifiers,
    }
}

fn assert_mouse_forwarded_once(
    outcome: &ClientShellInput,
    kind: crate::protocol::ClientMouseKind,
    column: u16,
    modifiers: KeyModifiers,
) {
    let inputs: Vec<_> = outcome
        .requests
        .iter()
        .filter_map(|request| match request {
            ClientMessage::ClientShellPaneInput { pane_id, events } => Some((pane_id, events)),
            _ => None,
        })
        .collect();
    let [(pane_id, events)] = inputs.as_slice() else {
        panic!("mouse gesture must reach one pane exactly once");
    };
    assert_eq!(pane_id.as_str(), "pane_1");
    assert!(matches!(
        events.as_slice(),
        [ClientPaneInputEvent::Mouse {
            kind: actual,
            position: ClientMousePosition::Cell { column: actual_column, row: 0 },
            modifiers: actual_modifiers,
            ..
        }] if *actual == kind && *actual_column == column && *actual_modifiers == modifiers.bits()
    ));
}

#[test]
fn first_routed_click_is_learned_behind_pending_typing_and_release_preserves_it() {
    let mut state = remote_mouse_shell(true);
    state.handle_input_bytes(b"cd");
    let authoritative = state.pane_surface.as_ref().unwrap().frame.clone();
    let down = mouse_at(
        &state,
        MouseEventKind::Down(MouseButton::Left),
        1,
        KeyModifiers::empty(),
    );
    let click = state.handle_raw_events(vec![RawInputEvent::Mouse(down)]);
    assert_mouse_forwarded_once(
        &click,
        crate::protocol::ClientMouseKind::Down(crate::protocol::ClientMouseButton::Left),
        1,
        KeyModifiers::empty(),
    );
    assert!(state.input_prediction.has_pending());
    assert_eq!(state.pane_surface.as_ref().unwrap().frame, authoritative);

    let up = mouse_at(
        &state,
        MouseEventKind::Up(MouseButton::Left),
        1,
        KeyModifiers::empty(),
    );
    let release = state.handle_raw_events(vec![RawInputEvent::Mouse(up)]);
    assert_mouse_forwarded_once(
        &release,
        crate::protocol::ClientMouseKind::Up(crate::protocol::ClientMouseButton::Left),
        1,
        KeyModifiers::empty(),
    );
    assert!(state.input_prediction.has_pending(), "release is neutral");
    echo_at(&mut state, "abcd", 1);
    assert!(
        !state.input_prediction.has_pending(),
        "full ordered echo settles typing and click"
    );
    assert_eq!(state.handle_input_bytes(b"x").requests.len(), 1);
}

#[test]
fn local_text_selection_is_never_recorded_as_remote_cursor_input() {
    let mut state = remote_mouse_shell(false);
    state.handle_input_bytes(b"cd");
    let click = mouse_at(
        &state,
        MouseEventKind::Down(MouseButton::Left),
        1,
        KeyModifiers::empty(),
    );
    let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(click)]);
    assert!(state.selection.is_some());
    assert!(!state.input_prediction.has_pending());
    assert!(!outcome
        .requests
        .iter()
        .any(|request| matches!(request, ClientMessage::ClientShellPaneInput { .. })));
    assert_eq!(
        state
            .pane_surface
            .as_ref()
            .unwrap()
            .frame
            .cursor
            .as_ref()
            .unwrap()
            .x,
        2
    );
}

#[test]
fn modified_application_click_falls_back_but_keeps_canonical_event_once() {
    let mut state = remote_mouse_shell(true);
    state.handle_input_bytes(b"cd");
    let click = mouse_at(
        &state,
        MouseEventKind::Down(MouseButton::Left),
        1,
        KeyModifiers::ALT,
    );
    let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(click)]);
    assert_mouse_forwarded_once(
        &outcome,
        crate::protocol::ClientMouseKind::Down(crate::protocol::ClientMouseButton::Left),
        1,
        KeyModifiers::ALT,
    );
    assert!(!state.input_prediction.has_pending());
}

#[test]
fn popup_mouse_target_does_not_modify_underlying_pane_prediction() {
    let mut state = remote_mouse_shell(true);
    state.handle_input_bytes(b"cd");
    let mut popup = state.hits.panes[0].clone();
    popup.popup = true;
    popup.pane_id = "terminal-popup".into();
    state.popup_terminal_id = Some("terminal-popup".into());
    let click = mouse_at(
        &state,
        MouseEventKind::Down(MouseButton::Left),
        1,
        KeyModifiers::empty(),
    );
    let mut outcome = ClientShellInput::default();
    state.push_pane_mouse_event(&popup, click, KeyModifiers::empty(), &mut outcome);
    assert!(!state.input_prediction.has_pending());
    assert!(matches!(
        outcome.requests.as_slice(),
        [ClientMessage::ClientShellPopupInput { terminal_id, events }]
            if terminal_id == "terminal-popup"
                && matches!(events.as_slice(), [ClientPaneInputEvent::Mouse { .. }])
    ));
}
