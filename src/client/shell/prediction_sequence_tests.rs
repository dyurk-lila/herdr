fn sequence_software_at(text: &str, x: u16) -> PaneSurfaceFrame {
    let mut frame = software_cursor_surface(text);
    let end = text.chars().count();
    let caret = frame.frame.cells[30 + end].clone();
    let padding = frame.frame.cells[59].clone();
    frame.frame.cells[30 + end] = padding;
    let cell = &mut frame.frame.cells[30 + usize::from(x)];
    cell.fg = caret.fg;
    cell.bg = caret.bg;
    cell.modifier = caret.modifier;
    at_cursor(frame, x)
}

#[test]
fn sequence_software_caret_middle_unicode_edits_preserve_text_and_partial_echoes() {
    let now = Instant::now();
    let mut prediction = InputPrediction::default();
    record(&mut prediction, &software_cursor_surface(""), "abcdef", now);
    let base = software_cursor_surface("abcdef");
    prediction.observe(&base, now + Duration::from_millis(10));
    for code in [ClientKeyCode::Left, ClientKeyCode::Left] {
        assert!(prediction.record_input(&base, "pane", &key(code), now));
    }
    let moved = rendered(&prediction, &base);
    assert!(row_text(&moved).starts_with("abcdef"));
    assert_eq!(moved.cells[34].symbol, "e");
    assert_eq!(moved.cells[34].bg, base.frame.cells[36].bg);
    assert_eq!(moved.cursor.as_ref().unwrap().x, 4);
    record(&mut prediction, &base, "é", now);
    for code in [
        ClientKeyCode::Delete,
        ClientKeyCode::Right,
        ClientKeyCode::Left,
        ClientKeyCode::End,
    ] {
        assert!(prediction.record_input(&base, "pane", &key(code), now));
    }
    record(&mut prediction, &base, "λ", now);
    let expected = rendered(&prediction, &base);
    assert!(row_text(&expected).starts_with("abcdéfλ "));
    assert_eq!(expected.cursor.as_ref().unwrap().x, 7);
    assert_eq!(row_text(&base.frame).trim_end(), "abcdef");
    for partial in [
        sequence_software_at("abcdef", 4),
        sequence_software_at("abcdéef", 5),
    ] {
        prediction.observe(&partial, now + Duration::from_millis(100));
        let frame = rendered(&prediction, &partial);
        assert!(row_text(&frame).starts_with("abcdéfλ "));
        assert_eq!(frame.cursor.as_ref().unwrap().x, 7);
        assert_eq!(frame.cells[37], expected.cells[37]);
    }
    let complete = software_cursor_surface("abcdéfλ");
    prediction.observe(&complete, now + Duration::from_millis(150));
    assert!(!prediction.has_pending());
    assert_eq!(rendered(&prediction, &complete), complete.frame);
}

fn sequence_click(column: u16) -> ClientPaneInputEvent {
    ClientPaneInputEvent::Mouse {
        kind: ClientMouseKind::Down(ClientMouseButton::Left),
        position: ClientMousePosition::Cell { column, row: 1 },
        geometry: None,
        modifiers: 0,
        lines: 0,
    }
}

#[test]
fn sequence_learned_click_composes_with_queued_text_backspace_and_end() {
    let now = Instant::now();
    let mut initial = surface("$ ");
    initial.panes[0].mouse_reporting = true;
    let mut base = surface("$ abcd");
    base.panes[0].mouse_reporting = true;
    let mut prediction = InputPrediction::default();
    record(&mut prediction, &initial, "abcd", now);
    prediction.observe(&base, now + Duration::from_millis(10));
    prediction.record_input(&base, "pane", &sequence_click(3), now);
    assert_eq!(rendered(&prediction, &base), base.frame);
    let clicked = at_cursor(base, 3);
    prediction.observe(&clicked, now + Duration::from_millis(20));
    assert!(!prediction.has_pending());
    let next = now + MULTI_CLICK_WINDOW;
    record(&mut prediction, &clicked, "xy", next);
    prediction.record_input(&clicked, "pane", &key(ClientKeyCode::Backspace), next);
    assert!(prediction.record_input(&clicked, "pane", &sequence_click(5), next));
    prediction.record_input(&clicked, "pane", &key(ClientKeyCode::End), next);
    record(&mut prediction, &clicked, "z", next);
    let frame = rendered(&prediction, &clicked);
    assert!(row_text(&frame).starts_with("$ axbcdz "));
    assert_eq!(frame.cursor.as_ref().unwrap().x, 8);
    let mut partial = at_cursor(surface("$ axybcd"), 5);
    partial.panes[0].mouse_reporting = true;
    prediction.observe(&partial, next + Duration::from_millis(10));
    assert!(row_text(&rendered(&prediction, &partial)).starts_with("$ axbcdz "));
    let mut complete = surface("$ axbcdz");
    complete.panes[0].mouse_reporting = true;
    prediction.observe(&complete, next + Duration::from_millis(20));
    assert!(!prediction.has_pending());
    assert_eq!(rendered(&prediction, &complete), complete.frame);
}

#[test]
fn sequence_second_click_inside_multi_click_window_removes_prediction() {
    let now = Instant::now();
    let mut prediction = InputPrediction::default();
    let mut initial = surface("$ ");
    initial.panes[0].mouse_reporting = true;
    record(&mut prediction, &initial, "abcd", now);
    let mut base = surface("$ abcd");
    base.panes[0].mouse_reporting = true;
    prediction.observe(&base, now + Duration::from_millis(10));
    prediction.record_input(&base, "pane", &sequence_click(3), now);
    let clicked = at_cursor(base, 3);
    prediction.observe(&clicked, now + Duration::from_millis(20));
    let next = now + MULTI_CLICK_WINDOW;
    assert!(prediction.record_input(&clicked, "pane", &sequence_click(4), next));
    assert_eq!(
        rendered(&prediction, &clicked).cursor.as_ref().unwrap().x,
        4
    );
    assert!(prediction.record_input(
        &clicked,
        "pane",
        &sequence_click(4),
        next + MULTI_CLICK_WINDOW - Duration::from_millis(1),
    ));
    assert!(!prediction.has_pending());
    assert_eq!(rendered(&prediction, &clicked), clicked.frame);
}

fn sequence_warm_codex(now: Instant, initial: &PaneSurfaceFrame) -> InputPrediction {
    let mut prediction = InputPrediction::default();
    prediction.set_agent_context(Some(crate::detect::Agent::Codex));
    assert!(!record(&mut prediction, initial, "abc", now));
    assert_eq!(rendered(&prediction, initial), initial.frame);
    let echoed = surface("› abc");
    prediction.observe(&echoed, now + Duration::from_millis(10));
    assert!(!prediction.has_pending());
    prediction.record_input(&echoed, "pane", &key(ClientKeyCode::Enter), now);
    assert!(prediction.warm_prompt.is_some());
    prediction
}

#[test]
fn sequence_routed_enter_reuses_exact_codex_prompt_on_the_first_character() {
    let now = Instant::now();
    let initial = surface("› ");
    let mut prediction = sequence_warm_codex(now, &initial);
    // The client prepares Enter before routing the same key to the pane.
    assert!(!prediction.record_input(&initial, "pane", &key(ClientKeyCode::Enter), now));
    assert!(record(&mut prediction, &initial, "é", now));
    assert!(row_text(&rendered(&prediction, &initial)).starts_with("› é "));
    assert_eq!(initial.frame, surface("› ").frame);
}

#[test]
fn sequence_warm_prompt_rejects_row_geometry_agent_and_boot_changes() {
    let now = Instant::now();
    let initial = surface("› ");
    for variation in 0..4 {
        let mut prediction = sequence_warm_codex(now, &initial);
        let mut changed = initial.clone();
        match variation {
            0 => changed.frame.cells[40].fg = 5,
            1 => changed.panes[0].inner_rect.width -= 1,
            2 => {
                prediction.set_agent_context(Some(crate::detect::Agent::Claude));
            }
            3 => changed.boot_id = "next-boot".into(),
            _ => unreachable!(),
        }
        assert!(
            !record(&mut prediction, &changed, "z", now),
            "case {variation}"
        );
        assert_eq!(
            rendered(&prediction, &changed),
            changed.frame,
            "case {variation}"
        );
    }
}

#[test]
fn sequence_placeholder_disappears_only_after_echo_and_is_reused_after_enter() {
    let now = Instant::now();
    let initial = at_cursor(surface("› Write a message"), 2);
    let mut prediction = sequence_warm_codex(now, &initial);
    assert!(record(&mut prediction, &initial, "λ", now));
    let frame = rendered(&prediction, &initial);
    assert_eq!(row_text(&frame).trim_end(), "› λ");
    assert_eq!(row_text(&initial.frame).trim_end(), "› Write a message");
    let complete = surface("› λ");
    prediction.observe(&complete, now + Duration::from_millis(20));
    assert!(!prediction.has_pending());
    assert_eq!(rendered(&prediction, &complete), complete.frame);
}

fn sequence_profile_bank(now: Instant) -> Profiles {
    let endpoint = crate::client::endpoint::ClientEndpointId::Local;
    let mut prediction = InputPrediction::default();
    prediction.set_machine_target(endpoint.clone(), "host-a");
    prediction.select_machine(&endpoint);
    prediction.set_agent_context(Some(crate::detect::Agent::Codex));
    record(&mut prediction, &surface("› "), "abcd", now);
    prediction.observe(&surface("› abcd"), now + Duration::from_millis(10));
    let updates = prediction.take_profile_updates();
    assert!(!updates.is_empty());
    let mut bank = Profiles::default();
    for update in updates {
        bank.apply(&update);
    }
    bank
}

fn sequence_with_profiles(
    bank: Profiles,
    target: &str,
    agent: crate::detect::Agent,
) -> InputPrediction {
    let endpoint = crate::client::endpoint::ClientEndpointId::Local;
    let mut prediction = InputPrediction::default();
    prediction.set_profiles(bank);
    prediction.set_machine_target(endpoint.clone(), target);
    prediction.select_machine(&endpoint);
    prediction.set_agent_context(Some(agent));
    prediction
}

#[test]
fn sequence_saved_profile_reuses_matching_prompt_in_a_new_client_and_boot() {
    let now = Instant::now();
    let bank = sequence_profile_bank(now);
    let bank = Profiles::from_json(&bank.to_json().unwrap()).unwrap();
    let mut prediction = sequence_with_profiles(bank, "host-a", crate::detect::Agent::Codex);
    let mut initial = surface("› ");
    initial.boot_id = "new-boot".into();
    initial.panes[0].pane_id = "new-pane".into();
    assert!(prediction.record_input(&initial, "new-pane", &key(ClientKeyCode::Char('z')), now));
    assert_eq!(row_text(&rendered(&prediction, &initial)).trim_end(), "› z");
    assert_eq!(initial.frame, surface("› ").frame);
}

#[test]
fn sequence_saved_profile_keeps_machine_agent_and_prompt_fingerprint_scopes() {
    let now = Instant::now();
    let bank = sequence_profile_bank(now);
    for (target, agent, changed_row) in [
        ("host-b", crate::detect::Agent::Codex, false),
        ("host-a", crate::detect::Agent::Claude, false),
        ("host-a", crate::detect::Agent::Codex, true),
    ] {
        let mut prediction = sequence_with_profiles(bank.clone(), target, agent);
        let mut initial = surface("› ");
        if changed_row {
            initial.frame.cells[40].bg = 5;
        }
        assert!(!record(&mut prediction, &initial, "z", now));
        assert_eq!(rendered(&prediction, &initial), initial.frame);
    }
}

#[test]
fn sequence_saved_profile_derives_bounds_from_the_current_draft_after_fresh_echo() {
    let now = Instant::now();
    let mut prediction = sequence_with_profiles(
        sequence_profile_bank(now),
        "host-a",
        crate::detect::Agent::Codex,
    );
    let initial = at_cursor(surface("› abcdef"), 4);
    assert!(!record(&mut prediction, &initial, "z", now));
    assert_eq!(rendered(&prediction, &initial), initial.frame);
    let echoed = at_cursor(surface("› abzcdef"), 5);
    prediction.observe(&echoed, now + Duration::from_millis(10));
    assert!(!prediction.has_pending());
    assert!(prediction.record_input(&echoed, "pane", &key(ClientKeyCode::End), now));
    assert_eq!(rendered(&prediction, &echoed).cursor.as_ref().unwrap().x, 9);
    record(&mut prediction, &echoed, "λ", now);
    assert_eq!(
        row_text(&rendered(&prediction, &echoed)).trim_end(),
        "› abzcdefλ"
    );
    assert_eq!(row_text(&echoed.frame).trim_end(), "› abzcdef");
}

#[test]
fn sequence_late_contradiction_invalidates_a_shared_profile_after_visible_expiry() {
    let now = Instant::now();
    let bank = sequence_profile_bank(now);
    let mut prediction = sequence_with_profiles(bank, "host-a", crate::detect::Agent::Codex);
    let initial = surface("› ");
    assert!(record(&mut prediction, &initial, "z", now));
    assert!(prediction.expire(now + Duration::from_secs(1)));
    assert_eq!(rendered(&prediction, &initial), initial.frame);
    prediction.observe(&surface("› other"), now + Duration::from_secs(1));
    let updates = prediction.take_profile_updates();
    assert!(matches!(
        updates.as_slice(),
        [ProfileUpdate::Invalidate { .. }]
    ));
    assert!(!record(
        &mut prediction,
        &initial,
        "z",
        now + Duration::from_secs(2)
    ));
}

#[test]
fn sequence_profile_reset_fences_old_refresh_and_warm_prompt_reuse() {
    let now = Instant::now();
    let bank = sequence_profile_bank(now);
    let mut prediction =
        sequence_with_profiles(bank.clone(), "host-a", crate::detect::Agent::Codex);
    let initial = surface("› ");
    assert!(record(&mut prediction, &initial, "z", now));
    assert!(prediction.pause_profiles_until_epoch(2));
    prediction.pause_profiles_until_epoch(1);
    prediction.set_profiles(bank.clone());
    assert!(!record(&mut prediction, &initial, "z", now));
    let mut reset = bank.clone();
    reset.reset_to_epoch(2);
    prediction.set_profiles(reset);
    prediction.clear();
    prediction.set_profiles(bank);
    assert!(!record(&mut prediction, &initial, "z", now));
}

#[test]
fn sequence_changed_editor_word_behavior_invalidates_and_relearns_shared_profile() {
    let now = Instant::now();
    let initial = surface("› ");
    let body = "prefix src/file.txt";
    let full = surface("› prefix src/file.txt");
    let whole_word_deleted = surface("› prefix ");
    let suffix_deleted = surface("› prefix src/file.");
    let gesture = word_key(WordGesture::ControlW);
    let mut bank = sequence_profile_bank(now);
    let mut original = sequence_with_profiles(bank.clone(), "host-a", crate::detect::Agent::Codex);
    assert!(record(&mut original, &initial, body, now));
    original.observe(&full, now + Duration::from_millis(10));
    original.record_input(&full, "pane", &gesture, now + Duration::from_millis(20));
    assert_eq!(rendered(&original, &full), full.frame);
    original.observe(&whole_word_deleted, now + Duration::from_millis(30));
    for update in original.take_profile_updates() {
        bank.apply(&update);
    }
    let machine = super::super::prediction_profiles::machine_key("host-a");
    assert_eq!(
        bank.profile(&machine, "codex").unwrap().word_rules[0].agreed_start(body),
        Some(7)
    );

    let restored = Profiles::from_json(&bank.to_json().unwrap()).unwrap();
    let mut changed = sequence_with_profiles(restored, "host-a", crate::detect::Agent::Codex);
    assert!(record(&mut changed, &initial, body, now));
    changed.observe(&full, now + Duration::from_millis(40));
    changed.record_input(&full, "pane", &gesture, now + Duration::from_millis(50));
    assert_eq!(row_text(&rendered(&changed, &full)).trim_end(), "› prefix");
    changed.observe(&suffix_deleted, now + Duration::from_millis(60));
    assert_eq!(rendered(&changed, &suffix_deleted), suffix_deleted.frame);
    let updates = changed.take_profile_updates();
    assert!(matches!(
        updates.as_slice(),
        [ProfileUpdate::Invalidate { .. }]
    ));
    for update in updates {
        assert!(bank.apply(&update));
    }
    assert!(bank.profile(&machine, "codex").is_none());
    assert_eq!(bank.epoch(), 1);

    changed.record_input(&suffix_deleted, "pane", &key(ClientKeyCode::Enter), now);
    assert!(!record(&mut changed, &initial, body, now));
    assert_eq!(rendered(&changed, &initial), initial.frame);
    changed.observe(&full, now + Duration::from_millis(70));
    changed.record_input(&full, "pane", &gesture, now + Duration::from_millis(80));
    assert_eq!(rendered(&changed, &full), full.frame);
    changed.observe(&suffix_deleted, now + Duration::from_millis(90));
    for update in changed.take_profile_updates() {
        bank.apply(&update);
    }
    assert_eq!(
        bank.profile(&machine, "codex").unwrap().word_rules[0].agreed_start(body),
        Some(16)
    );
    record(
        &mut changed,
        &suffix_deleted,
        "next",
        now + Duration::from_millis(100),
    );
    changed.record_input(
        &suffix_deleted,
        "pane",
        &gesture,
        now + Duration::from_millis(110),
    );
    assert!(changed.has_pending());
    assert_eq!(
        row_text(&rendered(&changed, &suffix_deleted)).trim_end(),
        "› prefix src/file."
    );
}
