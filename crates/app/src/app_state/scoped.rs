//! What a workspace's agent reaches, and its commands run as that workspace
//! ([`crate::agent::Scope::Workspace`]).

use std::time::{Duration, Instant};

use ghostrealm_core::{SurfaceId, VtabId};

use super::AppState;

impl AppState {
    /// Run `f`, an agent's command, as workspace `vt`: the active workspace
    /// for it ([`Self::active`] and all that derives from it), while the
    /// person's stays. A file job it starts runs in the background (no
    /// inline wait), so the UI never blocks on it.
    ///
    /// A question it raises is shown to the person marked late (keys ignored
    /// for a moment, Enter cancels), naming `vt`: it came while they may be
    /// typing elsewhere. One that would replace a question already open is
    /// withdrawn, and `Err` says why; nothing else ran, since asking is all
    /// such a command does.
    pub(crate) fn acting_as<T>(&mut self, vt: VtabId, f: impl FnOnce(&mut Self) -> T) -> Result<T, String> {
        let open = self.confirm.clone();
        let acting = self.acting.replace(vt);
        let wait = std::mem::replace(&mut self.file_job_wait, Duration::ZERO);
        let out = f(self);
        self.acting = acting;
        self.file_job_wait = wait;
        match (open, &mut self.confirm) {
            (None, Some(asked)) => {
                let name = self.tree.vtab(vt).map(|v| v.name.clone()).unwrap_or_default();
                asked.message = format!("From workspace \u{201c}{name}\u{201d}: {}", asked.message);
                asked.late = Some(Instant::now());
            }
            (Some(open), Some(asked)) if *asked != open => {
                self.confirm = Some(open);
                return Err("a question is open in the window; try again once the person has answered it".into());
            }
            _ => {}
        }
        Ok(out)
    }

    /// The workspace the running command acts as for an agent, if it does.
    pub(crate) fn agent_scope(&self) -> Option<VtabId> {
        self.acting
    }

    /// Whether workspace `vt`'s agent reaches surface `sid`: it is in `vt`'s
    /// panes and its view opts in ([`crate::plugin::View::agent_reachable`]).
    pub(crate) fn agent_reaches(&self, vt: VtabId, sid: SurfaceId) -> bool {
        self.vtab_of_surface(sid) == Some(vt) && self.views.get(&sid).is_some_and(|v| v.agent_reachable())
    }

    /// Whether every surface in `vt`'s focused pane is empty or one its agent
    /// reaches: closing it takes nothing that isn't the agent's.
    pub(crate) fn focused_pane_reachable(&self, vt: VtabId) -> bool {
        let Some(v) = self.tree.vtab(vt) else { return false };
        let Some(pane) = v.panes().into_iter().find(|p| p.id == v.focused_pane) else { return false };
        pane.surfaces.iter().all(|s| !self.views.contains_key(&s.id) || self.agent_reaches(vt, s.id))
    }

    /// The tree as workspace `vt`'s agent sees it.
    pub(crate) fn describe_for(&self, vt: VtabId) -> String {
        self.describe_tree(Some(vt))
    }
}
