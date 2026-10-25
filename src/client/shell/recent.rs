//! Most recently used workspaces across every machine, and the switcher the
//! `recent_workspace` key opens over them: a list in recent order with the
//! target highlighted. Further presses move the highlight and letting go of the
//! modifier switches, like Alt+Tab for windows. A terminal that cannot report the
//! release switches after a pause in the presses instead.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers, ModifierKeyCode};
use serde::{Deserialize, Serialize};

use super::*;

const HISTORY_LIMIT: usize = 64;
/// Without key release reports, the switcher switches this long after the last press.
const IDLE_COMMIT: Duration = Duration::from_millis(900);
/// With them, a switcher that never hears the release (the key was let go while
/// another window had focus, say) closes without switching after this long.
const ABANDON_AFTER: Duration = Duration::from_secs(15);

/// A workspace on one machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RecentWorkspace {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) workspace_id: String,
}

#[derive(Default)]
pub(super) struct RecentWorkspaces {
    /// Most recently used first.
    history: Vec<RecentWorkspace>,
    /// Where the history is kept between runs. Unset in tests, which must never
    /// write to the user's state directory.
    store: Option<PathBuf>,
}

/// One history entry as stored on disk.
#[derive(Serialize, Deserialize)]
struct SavedWorkspace {
    endpoint: String,
    workspace: String,
}

fn endpoint_from_storage_key(key: &str) -> Option<ClientEndpointId> {
    if key == "local" {
        return Some(ClientEndpointId::Local);
    }
    let profile_id = key.strip_prefix("ssh:")?;
    crate::client::endpoint::ProfileId::parse(profile_id)
        .ok()
        .map(ClientEndpointId::Ssh)
}

pub(super) fn history_path() -> PathBuf {
    crate::config::state_dir()
        .join("client")
        .join("recent-workspaces.json")
}

impl RecentWorkspaces {
    /// History kept in `path`, which is read now and rewritten whenever it changes,
    /// so a new client starts with the workspaces the last one used.
    pub(super) fn persisted(path: PathBuf) -> Self {
        let mut history: Vec<RecentWorkspace> = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Vec<SavedWorkspace>>(&bytes).ok())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|saved| {
                Some(RecentWorkspace {
                    endpoint_id: endpoint_from_storage_key(&saved.endpoint)?,
                    workspace_id: saved.workspace,
                })
            })
            .collect();
        history.truncate(HISTORY_LIMIT);
        Self {
            history,
            store: Some(path),
        }
    }

    fn save(&self) {
        let Some(path) = &self.store else {
            return;
        };
        let saved: Vec<SavedWorkspace> = self
            .history
            .iter()
            .map(|entry| SavedWorkspace {
                endpoint: entry.endpoint_id.storage_key(),
                workspace: entry.workspace_id.clone(),
            })
            .collect();
        let write = || -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let temp = path.with_extension("json.tmp");
            std::fs::write(&temp, serde_json::to_vec(&saved)?)?;
            std::fs::rename(&temp, path)
        };
        if let Err(error) = write() {
            tracing::warn!(error = %error, "failed to save recent workspaces");
        }
    }

    /// The focused workspace on the active machine changed.
    pub(super) fn visit(&mut self, workspace: RecentWorkspace) {
        if self.history.first() == Some(&workspace) {
            return;
        }
        self.history.retain(|entry| entry != &workspace);
        self.history.insert(0, workspace);
        self.history.truncate(HISTORY_LIMIT);
        self.save();
    }

    /// The workspace shown, then the ones used before it, then any others in
    /// sidebar order, so there is somewhere to go even with no history yet.
    /// `existing` is every workspace that is still there on a reachable machine.
    fn candidates(
        &self,
        current: Option<RecentWorkspace>,
        existing: &[RecentWorkspace],
    ) -> Vec<RecentWorkspace> {
        let mut list: Vec<RecentWorkspace> = current.iter().cloned().collect();
        for entry in self.history.iter().chain(existing) {
            if existing.contains(entry) && !list.contains(entry) {
                list.push(entry.clone());
            }
        }
        list
    }
}

/// What a timer tick should do with the open switcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SwitcherTimeout {
    Wait,
    Commit,
    Abandon,
}

/// The open switcher: the workspaces in recent order, frozen when it opened, with
/// the first one the workspace shown now.
#[derive(Debug)]
pub(super) struct RecentSwitcher {
    pub(super) list: Vec<RecentWorkspace>,
    pub(super) selected: usize,
    last_input: Instant,
    /// The terminal reports key releases, so letting go of the modifier ends it.
    commit_on_release: bool,
}

impl RecentSwitcher {
    /// None when there is no other workspace to go to.
    fn open(list: Vec<RecentWorkspace>, now: Instant, commit_on_release: bool) -> Option<Self> {
        (list.len() >= 2).then_some(Self {
            list,
            selected: 1,
            last_input: now,
            commit_on_release,
        })
    }

    fn step(&mut self, now: Instant, backwards: bool) {
        let len = self.list.len();
        self.selected = if backwards {
            (self.selected + len - 1) % len
        } else {
            (self.selected + 1) % len
        };
        self.last_input = now;
    }

    fn target(&self) -> &RecentWorkspace {
        &self.list[self.selected]
    }

    fn timeout(&self, now: Instant) -> SwitcherTimeout {
        let idle = now.saturating_duration_since(self.last_input);
        if self.commit_on_release {
            if idle >= ABANDON_AFTER {
                SwitcherTimeout::Abandon
            } else {
                SwitcherTimeout::Wait
            }
        } else if idle >= IDLE_COMMIT {
            SwitcherTimeout::Commit
        } else {
            SwitcherTimeout::Wait
        }
    }

    fn deadline(&self) -> Instant {
        self.last_input
            + if self.commit_on_release {
                ABANDON_AFTER
            } else {
                IDLE_COMMIT
            }
    }
}

/// Letting go of one of these ends the switcher; Shift is held to go backwards, so
/// it does not.
fn is_switching_modifier(code: &KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Modifier(
            ModifierKeyCode::LeftControl
                | ModifierKeyCode::RightControl
                | ModifierKeyCode::LeftAlt
                | ModifierKeyCode::RightAlt
                | ModifierKeyCode::LeftSuper
                | ModifierKeyCode::RightSuper
                | ModifierKeyCode::LeftMeta
                | ModifierKeyCode::RightMeta
                | ModifierKeyCode::LeftHyper
                | ModifierKeyCode::RightHyper
        )
    )
}

impl ClientShellState {
    /// Every workspace on a machine that is online, as the history knows them.
    fn existing_workspaces(&self) -> Vec<RecentWorkspace> {
        self.endpoints
            .iter()
            .filter(|endpoint| endpoint.status == ClientEndpointStatus::Online)
            .flat_map(|endpoint| {
                endpoint.snapshot.iter().flat_map(|snapshot| {
                    snapshot.workspaces.iter().map(|workspace| RecentWorkspace {
                        endpoint_id: endpoint.endpoint_id.clone(),
                        workspace_id: workspace.workspace_id.clone(),
                    })
                })
            })
            .collect()
    }

    /// The workspace shown now, on the active machine.
    pub(super) fn current_recent_workspace(&self) -> Option<RecentWorkspace> {
        let workspace_id = self.snapshot.as_deref()?.focused_workspace_id.clone()?;
        Some(RecentWorkspace {
            endpoint_id: self.active_endpoint_id.clone(),
            workspace_id,
        })
    }

    /// Keep the history in the client state directory from now on.
    pub(crate) fn restore_recent_workspaces(&mut self) {
        self.recent = RecentWorkspaces::persisted(history_path());
    }

    /// The `recent_workspace` key: open the switcher on the previous workspace.
    pub(super) fn open_recent_switcher(&mut self, outcome: &mut ClientShellInput) {
        let list = self
            .recent
            .candidates(self.current_recent_workspace(), &self.existing_workspaces());
        let Some(switcher) =
            RecentSwitcher::open(list, Instant::now(), self.host_reports_key_releases)
        else {
            return;
        };
        self.overlay = Some(ClientShellOverlay::RecentSwitcher(switcher));
        outcome.repaint = true;
    }

    pub(super) fn recent_switcher_open(&self) -> bool {
        matches!(self.overlay, Some(ClientShellOverlay::RecentSwitcher(_)))
    }

    /// Whether `key` is the `recent_workspace` key again, and if so whether it is
    /// held with Shift, which goes backwards.
    fn recent_binding_press(&self, key: &crate::input::TerminalKey) -> Option<bool> {
        let is_binding = |key: &crate::input::TerminalKey| {
            matches!(
                crate::input::resolve_direct_binding(&self.config.keybinds.keybinds, key),
                Some(crate::input::KeybindMatch::Action(
                    crate::input::KeybindAction::RecentWorkspace
                ))
            )
        };
        if is_binding(key) {
            return Some(false);
        }
        let without_shift = key.modifiers.difference(KeyModifiers::SHIFT);
        let unshifted = if key.code == KeyCode::BackTab {
            crate::input::TerminalKey::new(KeyCode::Tab, without_shift)
        } else if key.modifiers.contains(KeyModifiers::SHIFT) {
            key.clone().with_modifiers(without_shift)
        } else {
            return None;
        };
        is_binding(&unshifted).then_some(true)
    }

    /// A key pressed while the switcher is open.
    pub(super) fn route_recent_switcher_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        match key.code {
            KeyCode::Esc => {
                self.overlay = None;
                outcome.repaint = true;
                return;
            }
            KeyCode::Enter => {
                self.commit_recent_switcher(outcome);
                return;
            }
            _ => {}
        }
        let backwards = match key.code {
            KeyCode::Up | KeyCode::Char('k') => Some(true),
            KeyCode::Down | KeyCode::Char('j') => Some(false),
            _ => self.recent_binding_press(key),
        };
        let Some(backwards) = backwards else {
            return;
        };
        if let Some(ClientShellOverlay::RecentSwitcher(switcher)) = self.overlay.as_mut() {
            switcher.step(Instant::now(), backwards);
            outcome.repaint = true;
        }
    }

    /// A key was let go. True when that ended the switcher, which then switched.
    pub(super) fn recent_switcher_key_released(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(ClientShellOverlay::RecentSwitcher(switcher)) = self.overlay.as_ref() else {
            return false;
        };
        if !switcher.commit_on_release || !is_switching_modifier(&key.code) {
            return false;
        }
        self.commit_recent_switcher(outcome);
        true
    }

    /// Close the switcher and go to the highlighted workspace.
    pub(super) fn commit_recent_switcher(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::RecentSwitcher(switcher)) = self.overlay.take() else {
            return;
        };
        outcome.repaint = true;
        let target = switcher.target().clone();
        if switcher.selected == 0 || !self.existing_workspaces().contains(&target) {
            return;
        }
        self.focus_or_activate(
            target.endpoint_id,
            ClientEndpointFocusTarget::Workspace(target.workspace_id),
            outcome,
        );
    }

    /// Close the switcher without switching.
    pub(super) fn cancel_recent_switcher(&mut self) -> bool {
        if !self.recent_switcher_open() {
            return false;
        }
        self.overlay = None;
        true
    }

    /// A timer tick: switch after a pause in the presses when the terminal cannot
    /// report the release, and give up on a switcher nothing has ended.
    pub(crate) fn tick_recent_switcher(&mut self, now: Instant, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::RecentSwitcher(switcher)) = self.overlay.as_ref() else {
            return;
        };
        match switcher.timeout(now) {
            SwitcherTimeout::Wait => {}
            SwitcherTimeout::Commit => self.commit_recent_switcher(outcome),
            SwitcherTimeout::Abandon => {
                self.overlay = None;
                outcome.repaint = true;
            }
        }
    }

    /// When the switcher next needs a timer tick.
    pub(crate) fn recent_switcher_deadline(&self) -> Option<Instant> {
        match self.overlay.as_ref() {
            Some(ClientShellOverlay::RecentSwitcher(switcher)) => Some(switcher.deadline()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::endpoint::ProfileId;

    fn workspace(machine: &str, id: &str) -> RecentWorkspace {
        RecentWorkspace {
            endpoint_id: if machine.is_empty() {
                ClientEndpointId::Local
            } else {
                ClientEndpointId::Ssh(ProfileId::parse("b".repeat(32)).unwrap())
            },
            workspace_id: id.to_string(),
        }
    }

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    fn all() -> Vec<RecentWorkspace> {
        vec![
            workspace("", "w1"),
            workspace("", "w2"),
            workspace("big", "w3"),
        ]
    }

    fn history(order: &[RecentWorkspace]) -> RecentWorkspaces {
        let mut recent = RecentWorkspaces::default();
        for entry in order.iter().rev() {
            recent.visit(entry.clone());
        }
        recent
    }

    #[test]
    fn candidates_are_the_current_workspace_then_the_recent_ones() {
        let recent = history(&[workspace("big", "w3"), workspace("", "w1")]);
        let list = recent.candidates(Some(workspace("big", "w3")), &all());
        // w3 is shown, w1 was used before it, w2 was never visited.
        assert_eq!(
            list,
            [
                workspace("big", "w3"),
                workspace("", "w1"),
                workspace("", "w2")
            ]
        );
    }

    #[test]
    fn workspaces_that_are_gone_are_left_out() {
        let recent = history(&[
            workspace("", "w2"),
            workspace("", "gone"),
            workspace("", "w1"),
        ]);
        let list = recent.candidates(Some(workspace("", "w2")), &all());
        assert_eq!(list[..2], [workspace("", "w2"), workspace("", "w1")]);
        assert!(!list.contains(&workspace("", "gone")));
    }

    #[test]
    fn a_fresh_client_falls_back_to_sidebar_order() {
        let recent = RecentWorkspaces::default();
        let list = recent.candidates(Some(workspace("", "w2")), &all());
        assert_eq!(
            list,
            [
                workspace("", "w2"),
                workspace("", "w1"),
                workspace("big", "w3")
            ]
        );
    }

    #[test]
    fn the_switcher_opens_on_the_previous_workspace_and_wraps_both_ways() {
        let base = Instant::now();
        let mut switcher = RecentSwitcher::open(all(), base, true).unwrap();
        assert_eq!(switcher.selected, 1);
        assert_eq!(switcher.target(), &workspace("", "w2"));
        switcher.step(at(base, 100), false);
        assert_eq!(switcher.target(), &workspace("big", "w3"));
        // Past the end it comes back to the workspace shown.
        switcher.step(at(base, 200), false);
        assert_eq!(switcher.selected, 0);
        switcher.step(at(base, 300), true);
        assert_eq!(switcher.target(), &workspace("big", "w3"));
    }

    #[test]
    fn a_lone_workspace_has_nowhere_to_go() {
        assert!(RecentSwitcher::open(vec![workspace("", "w1")], Instant::now(), true).is_none());
    }

    #[test]
    fn without_release_reports_it_switches_after_a_pause_in_the_presses() {
        let base = Instant::now();
        let mut switcher = RecentSwitcher::open(all(), base, false).unwrap();
        assert_eq!(switcher.timeout(at(base, 500)), SwitcherTimeout::Wait);
        // A press starts the pause again.
        switcher.step(at(base, 800), false);
        assert_eq!(switcher.timeout(at(base, 1_500)), SwitcherTimeout::Wait);
        assert_eq!(switcher.timeout(at(base, 1_800)), SwitcherTimeout::Commit);
        assert_eq!(switcher.deadline(), at(base, 800) + IDLE_COMMIT);
    }

    #[test]
    fn with_release_reports_only_a_stuck_switcher_gives_up() {
        let base = Instant::now();
        let switcher = RecentSwitcher::open(all(), base, true).unwrap();
        // Holding the modifier to read the list is fine for a long while.
        assert_eq!(switcher.timeout(at(base, 10_000)), SwitcherTimeout::Wait);
        assert_eq!(switcher.timeout(at(base, 16_000)), SwitcherTimeout::Abandon);
    }

    #[test]
    fn only_letting_go_of_a_switching_modifier_counts() {
        assert!(is_switching_modifier(&KeyCode::Modifier(
            ModifierKeyCode::LeftControl
        )));
        assert!(is_switching_modifier(&KeyCode::Modifier(
            ModifierKeyCode::RightAlt
        )));
        assert!(!is_switching_modifier(&KeyCode::Modifier(
            ModifierKeyCode::LeftShift
        )));
        assert!(!is_switching_modifier(&KeyCode::Tab));
    }

    #[test]
    fn history_survives_a_restart_and_ignores_unreadable_entries() {
        let dir = std::env::temp_dir().join(format!("herdr-recent-{}", std::process::id()));
        let path = dir.join("recent-workspaces.json");
        let _ = std::fs::remove_dir_all(&dir);

        let mut first_run = RecentWorkspaces::persisted(path.clone());
        first_run.visit(workspace("", "w1"));
        first_run.visit(workspace("big", "w3"));
        first_run.visit(workspace("", "w2"));

        let second_run = RecentWorkspaces::persisted(path.clone());
        let list = second_run.candidates(Some(workspace("", "w2")), &all());
        assert_eq!(list[1], workspace("big", "w3"));

        std::fs::write(&path, b"not json").expect("write garbage");
        let broken = RecentWorkspaces::persisted(path);
        let list = broken.candidates(Some(workspace("", "w2")), &all());
        assert_eq!(list[1], workspace("", "w1"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
