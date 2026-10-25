//! Most recently used workspaces across every machine, for the `recent_workspace`
//! key: one press goes to the previous workspace, further presses in quick
//! succession step back through the history, like cycling windows with Alt+Tab.
//! Repeat presses are told apart by a short window since the last press, because a
//! terminal reports no key release.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::*;

/// How long after a press the next one still continues the same cycle.
pub(super) const CYCLE_WINDOW: Duration = Duration::from_millis(700);
const HISTORY_LIMIT: usize = 64;

/// A workspace on one machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RecentWorkspace {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) workspace_id: String,
}

/// A run of presses. The history is frozen while it runs, so each press lands
/// one step further back, and only the workspace it ends on becomes the most
/// recent: the ones passed on the way are not visits.
struct Cycle {
    list: Vec<RecentWorkspace>,
    index: usize,
    last_press: Instant,
    /// Every workspace the cycle has jumped to. A machine can confirm a jump
    /// only after the next press, so their visits are not the user's own.
    jumped_to: Vec<RecentWorkspace>,
}

impl Cycle {
    fn landed(&self) -> &RecentWorkspace {
        &self.list[self.index]
    }

    fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_press) > CYCLE_WINDOW
    }
}

#[derive(Default)]
pub(super) struct RecentWorkspaces {
    /// Most recently used first.
    history: Vec<RecentWorkspace>,
    cycle: Option<Cycle>,
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
            cycle: None,
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

    fn promote(&mut self, workspace: RecentWorkspace) {
        if self.history.first() == Some(&workspace) {
            return;
        }
        self.history.retain(|entry| entry != &workspace);
        self.history.insert(0, workspace);
        self.history.truncate(HISTORY_LIMIT);
        self.save();
    }

    /// End a finished cycle: the workspace it landed on is now the most recent.
    fn settle(&mut self, now: Instant) {
        if self.cycle.as_ref().is_some_and(|cycle| cycle.expired(now)) {
            self.end_cycle();
        }
    }

    fn end_cycle(&mut self) {
        if let Some(cycle) = self.cycle.take() {
            let landed = cycle.landed().clone();
            self.promote(landed);
        }
    }

    /// The focused workspace on the active machine changed.
    pub(super) fn visit(&mut self, now: Instant, workspace: RecentWorkspace) {
        self.settle(now);
        if let Some(cycle) = &self.cycle {
            if cycle.jumped_to.contains(&workspace) {
                // This is a jump a press made, not a visit of its own.
                return;
            }
            self.end_cycle();
        }
        self.promote(workspace);
    }

    /// A press: the workspace to go to, if there is another one. `current` is the
    /// workspace shown now and `existing` every workspace that is still there on
    /// a machine that is reachable.
    pub(super) fn press(
        &mut self,
        now: Instant,
        current: Option<RecentWorkspace>,
        existing: &[RecentWorkspace],
    ) -> Option<RecentWorkspace> {
        self.settle(now);
        if let Some(cycle) = self.cycle.as_mut() {
            cycle.index = (cycle.index + 1) % cycle.list.len();
            cycle.last_press = now;
            let landed = cycle.landed().clone();
            cycle.jumped_to.push(landed.clone());
            return Some(landed);
        }
        // The workspace shown, then the ones used before it, then any others in
        // sidebar order, so a press has somewhere to go even with no history yet.
        let mut list: Vec<RecentWorkspace> = current.iter().cloned().collect();
        for entry in self.history.iter().chain(existing) {
            if existing.contains(entry) && !list.contains(entry) {
                list.push(entry.clone());
            }
        }
        if list.len() < 2 {
            return None;
        }
        let target = list[1].clone();
        self.cycle = Some(Cycle {
            list,
            index: 1,
            last_press: now,
            jumped_to: vec![target.clone()],
        });
        Some(target)
    }
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

    /// The `recent_workspace` key.
    pub(super) fn switch_recent_workspace(&mut self, outcome: &mut ClientShellInput) {
        let existing = self.existing_workspaces();
        let current = self.current_recent_workspace();
        let Some(target) = self.recent.press(Instant::now(), current, &existing) else {
            return;
        };
        self.focus_or_activate(
            target.endpoint_id,
            ClientEndpointFocusTarget::Workspace(target.workspace_id),
            outcome,
        );
        outcome.repaint = true;
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

    #[test]
    fn a_press_goes_to_the_previous_workspace_and_back() {
        let base = Instant::now();
        let mut recent = RecentWorkspaces::default();
        recent.visit(base, workspace("", "w1"));
        recent.visit(at(base, 10), workspace("", "w2"));

        let target = recent.press(at(base, 20_000), Some(workspace("", "w2")), &all());
        assert_eq!(target, Some(workspace("", "w1")));
        // The jump lands and is recorded; a later press returns to where it came from.
        recent.visit(at(base, 20_050), workspace("", "w1"));
        let back = recent.press(at(base, 40_000), Some(workspace("", "w1")), &all());
        assert_eq!(back, Some(workspace("", "w2")));
    }

    #[test]
    fn presses_in_quick_succession_step_further_back() {
        let base = Instant::now();
        let mut recent = RecentWorkspaces::default();
        for (i, ws) in [
            workspace("", "w1"),
            workspace("", "w2"),
            workspace("big", "w3"),
        ]
        .into_iter()
        .enumerate()
        {
            recent.visit(at(base, i as u64), ws);
        }
        let now = at(base, 10_000);
        let first = recent.press(now, Some(workspace("big", "w3")), &all());
        assert_eq!(first, Some(workspace("", "w2")));
        let second = recent.press(at(now, 300), Some(workspace("", "w2")), &all());
        assert_eq!(second, Some(workspace("", "w1")));
        // Past the end of the history it wraps to where the cycle started.
        let third = recent.press(at(now, 600), Some(workspace("", "w1")), &all());
        assert_eq!(third, Some(workspace("big", "w3")));
    }

    #[test]
    fn only_the_workspace_a_cycle_ends_on_becomes_the_most_recent() {
        let base = Instant::now();
        let mut recent = RecentWorkspaces::default();
        for (i, ws) in [
            workspace("", "w1"),
            workspace("", "w2"),
            workspace("big", "w3"),
        ]
        .into_iter()
        .enumerate()
        {
            recent.visit(at(base, i as u64), ws);
        }
        let now = at(base, 10_000);
        recent.press(now, Some(workspace("big", "w3")), &all()); // w2
        recent.press(at(now, 300), Some(workspace("", "w2")), &all()); // w1
        recent.visit(at(now, 350), workspace("", "w2"));
        recent.visit(at(now, 400), workspace("", "w1"));
        // After the cycle, w2 was only passed through: the previous one is w3.
        let later = at(now, 30_000);
        let target = recent.press(later, Some(workspace("", "w1")), &all());
        assert_eq!(target, Some(workspace("big", "w3")));
    }

    #[test]
    fn workspaces_that_are_gone_are_skipped() {
        let base = Instant::now();
        let mut recent = RecentWorkspaces::default();
        recent.visit(base, workspace("", "w1"));
        recent.visit(at(base, 1), workspace("", "gone"));
        recent.visit(at(base, 2), workspace("", "w2"));
        let target = recent.press(at(base, 9_000), Some(workspace("", "w2")), &all());
        assert_eq!(target, Some(workspace("", "w1")));
    }

    #[test]
    fn a_lone_workspace_has_nowhere_to_go() {
        let base = Instant::now();
        let mut recent = RecentWorkspaces::default();
        recent.visit(base, workspace("", "w1"));
        let only = [workspace("", "w1")];
        assert_eq!(
            recent.press(at(base, 5_000), Some(workspace("", "w1")), &only),
            None
        );
    }

    #[test]
    fn moving_elsewhere_during_a_cycle_ends_it() {
        let base = Instant::now();
        let mut recent = RecentWorkspaces::default();
        recent.visit(base, workspace("", "w1"));
        recent.visit(at(base, 1), workspace("", "w2"));
        let now = at(base, 5_000);
        recent.press(now, Some(workspace("", "w2")), &all()); // lands on w1
                                                              // The user picks another workspace by hand before settling.
        recent.visit(at(now, 200), workspace("big", "w3"));
        let target = recent.press(at(now, 20_000), Some(workspace("big", "w3")), &all());
        // w1 was landed on, so it is the previous one, not w2.
        assert_eq!(target, Some(workspace("", "w1")));
    }

    #[test]
    fn a_fresh_client_still_has_somewhere_to_go() {
        let base = Instant::now();
        let mut recent = RecentWorkspaces::default();
        recent.visit(base, workspace("", "w2"));
        // Nothing but the workspace shown is known, so go by sidebar order.
        let now = at(base, 5_000);
        let first = recent.press(now, Some(workspace("", "w2")), &all());
        assert_eq!(first, Some(workspace("", "w1")));
        let second = recent.press(at(now, 300), Some(workspace("", "w1")), &all());
        assert_eq!(second, Some(workspace("big", "w3")));
    }

    #[test]
    fn history_survives_a_restart_and_ignores_unreadable_entries() {
        let dir = std::env::temp_dir().join(format!("herdr-recent-{}", std::process::id()));
        let path = dir.join("recent-workspaces.json");
        let _ = std::fs::remove_dir_all(&dir);

        let base = Instant::now();
        let mut first_run = RecentWorkspaces::persisted(path.clone());
        first_run.visit(base, workspace("", "w1"));
        first_run.visit(at(base, 1), workspace("big", "w3"));
        first_run.visit(at(base, 2), workspace("", "w2"));

        let mut second_run = RecentWorkspaces::persisted(path.clone());
        let target = second_run.press(at(base, 9_000), Some(workspace("", "w2")), &all());
        assert_eq!(target, Some(workspace("big", "w3")));

        std::fs::write(&path, b"not json").expect("write garbage");
        let mut broken = RecentWorkspaces::persisted(path);
        assert_eq!(
            broken.press(at(base, 9_000), Some(workspace("", "w2")), &all()),
            Some(workspace("", "w1"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
