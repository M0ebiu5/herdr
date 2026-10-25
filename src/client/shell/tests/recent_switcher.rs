use super::*;
use crate::client::shell::recent::RecentWorkspace;
use crossterm::event::{KeyEventKind, ModifierKeyCode};

fn key(code: KeyCode, modifiers: KeyModifiers) -> crate::input::TerminalKey {
    crate::input::TerminalKey::new(code, modifiers)
}

fn ctrl_tab() -> crate::input::TerminalKey {
    key(KeyCode::Tab, KeyModifiers::CONTROL)
}

fn ctrl_released() -> crate::input::TerminalKey {
    key(
        KeyCode::Modifier(ModifierKeyCode::LeftControl),
        KeyModifiers::empty(),
    )
    .with_kind(KeyEventKind::Release)
}

fn local(workspace: &str) -> RecentWorkspace {
    RecentWorkspace {
        endpoint_id: ClientEndpointId::Local,
        workspace_id: workspace.into(),
    }
}

/// Local ws_1 (shown) and ws_2, and the remote's ws_1, used in that order:
/// remote, then ws_2, then ws_1.
fn state_with_history(host_reports_releases: bool) -> (ClientShellState, ClientEndpointId) {
    let (mut state, remote) = state_with_remote();
    let config: Config = toml::from_str("[keys]\nrecent_workspace = 'ctrl+tab'").unwrap();
    state.config = ClientShellConfig::from_config(&config);
    state.set_host_reports_key_releases(host_reports_releases);

    let mut projected = snapshot();
    let mut second = projected.workspaces[0].clone();
    second.workspace_id = "ws_2".into();
    second.number = 2;
    second.focused = false;
    projected.workspaces.push(second);
    state.set_snapshot(Box::new(projected));

    state.recent.visit(RecentWorkspace {
        endpoint_id: remote.clone(),
        workspace_id: "ws_1".into(),
    });
    state.recent.visit(local("ws_2"));
    state.recent.visit(local("ws_1"));
    (state, remote)
}

fn press(state: &mut ClientShellState, key: crate::input::TerminalKey) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.handle_key(key, &mut outcome);
    outcome
}

fn selected(state: &ClientShellState) -> Option<usize> {
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::RecentSwitcher(switcher)) => Some(switcher.selected),
        _ => None,
    }
}

#[test]
fn the_key_opens_the_list_on_the_previous_workspace_without_switching() {
    let (mut state, _) = state_with_history(true);
    let outcome = press(&mut state, ctrl_tab());
    assert_eq!(selected(&state), Some(1));
    assert!(outcome.repaint);
    assert!(outcome.actions.is_empty());
    assert!(outcome.requests.is_empty());
    let Some(ClientShellOverlay::RecentSwitcher(switcher)) = state.overlay.as_ref() else {
        panic!("switcher is open");
    };
    // Shown now first, then the workspaces in the order they were used.
    assert_eq!(switcher.list[0], local("ws_1"));
    assert_eq!(switcher.list[1], local("ws_2"));
}

#[test]
fn letting_go_of_ctrl_switches_to_the_highlighted_workspace_on_its_machine() {
    let (mut state, remote) = state_with_history(true);
    press(&mut state, ctrl_tab());
    press(&mut state, ctrl_tab());
    assert_eq!(selected(&state), Some(2));
    let outcome = press(&mut state, ctrl_released());
    assert!(state.overlay.is_none());
    assert!(
        matches!(outcome.actions.as_slice(), [ClientShellAction::ActivateEndpoint {
        endpoint_id, target: Some(ClientEndpointFocusTarget::Workspace(id)),
    }] if endpoint_id == &remote && id == "ws_1")
    );
}

#[test]
fn shift_goes_back_and_wraps_to_the_workspace_shown() {
    let (mut state, _) = state_with_history(true);
    press(&mut state, ctrl_tab());
    press(
        &mut state,
        key(KeyCode::Tab, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
    );
    assert_eq!(selected(&state), Some(0));
    // Highlighting the workspace already shown switches nowhere.
    let outcome = press(&mut state, ctrl_released());
    assert!(state.overlay.is_none());
    assert!(outcome.actions.is_empty());
    assert!(outcome.requests.is_empty());
}

#[test]
fn escape_closes_it_and_a_later_release_does_nothing() {
    let (mut state, _) = state_with_history(true);
    press(&mut state, ctrl_tab());
    press(&mut state, key(KeyCode::Esc, KeyModifiers::empty()));
    assert!(state.overlay.is_none());
    let outcome = press(&mut state, ctrl_released());
    assert!(outcome.actions.is_empty());
    assert!(outcome.requests.is_empty());
}

#[test]
fn enter_switches_without_waiting_for_the_release() {
    let (mut state, _) = state_with_history(true);
    press(&mut state, ctrl_tab());
    let outcome = press(&mut state, key(KeyCode::Enter, KeyModifiers::empty()));
    assert!(state.overlay.is_none());
    assert!(
        !outcome.actions.is_empty() || !outcome.requests.is_empty(),
        "a switch was requested"
    );
}

#[test]
fn releasing_shift_alone_keeps_it_open() {
    let (mut state, _) = state_with_history(true);
    press(&mut state, ctrl_tab());
    let shift_released = key(
        KeyCode::Modifier(ModifierKeyCode::LeftShift),
        KeyModifiers::empty(),
    )
    .with_kind(KeyEventKind::Release);
    press(&mut state, shift_released);
    assert_eq!(selected(&state), Some(1));
}

#[test]
fn without_release_reports_it_switches_after_a_pause() {
    let (mut state, remote) = state_with_history(false);
    let opened = std::time::Instant::now();
    press(&mut state, ctrl_tab());
    press(&mut state, ctrl_tab());
    let mut outcome = ClientShellInput::default();
    state.tick_recent_switcher(opened, &mut outcome);
    assert!(state.overlay.is_some(), "still within the pause");

    let later = opened + std::time::Duration::from_secs(5);
    state.tick_recent_switcher(later, &mut outcome);
    assert!(state.overlay.is_none());
    assert!(
        matches!(outcome.actions.as_slice(), [ClientShellAction::ActivateEndpoint {
        endpoint_id, target: Some(ClientEndpointFocusTarget::Workspace(id)),
    }] if endpoint_id == &remote && id == "ws_1")
    );
}

#[test]
fn losing_the_window_focus_cancels_it() {
    let (mut state, _) = state_with_history(true);
    press(&mut state, ctrl_tab());
    let outcome = state.handle_input_bytes(b"\x1b[O");
    assert!(state.overlay.is_none());
    assert!(outcome.repaint);
    assert!(outcome.actions.is_empty());
}

#[test]
fn the_terminal_is_asked_for_every_key_only_while_it_is_open() {
    let (mut state, _) = state_with_history(true);
    assert!(!state.host_keyboard_report_all_requested());
    press(&mut state, ctrl_tab());
    assert!(state.host_keyboard_report_all_requested());
    press(&mut state, key(KeyCode::Esc, KeyModifiers::empty()));
    assert!(!state.host_keyboard_report_all_requested());
}

#[test]
fn with_one_workspace_only_the_key_does_nothing() {
    let (mut state, _) = state_with_remote();
    let config: Config = toml::from_str("[keys]\nrecent_workspace = 'ctrl+tab'").unwrap();
    state.config = ClientShellConfig::from_config(&config);
    state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Online);
    // Take the remote machine away: nothing else is left to go to.
    state.set_endpoint_status(
        &ClientEndpointId::Ssh(ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap()),
        ClientEndpointStatus::Reconnecting,
    );
    let outcome = press(&mut state, ctrl_tab());
    assert!(state.overlay.is_none());
    assert!(outcome.actions.is_empty());
}
