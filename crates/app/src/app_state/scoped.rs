//! What a workspace's agent reaches, and its commands run as that workspace
//! ([`crate::agent::Scope::Workspace`]).

use std::path::PathBuf;
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
        match open {
            None => {
                let from = self.asked_from(vt);
                if let Some(asked) = &mut self.confirm {
                    asked.message.insert_str(0, &from);
                    asked.late = Some(Instant::now());
                }
            }
            Some(open) if self.confirm.as_ref().is_some_and(|asked| *asked != open) => {
                self.confirm = Some(open);
                return Err("a question is open in the window; try again once the person has answered it".into());
            }
            Some(_) => {}
        }
        Ok(out)
    }

    /// The start of a question workspace `vt`'s agent raised, naming `vt`.
    pub(super) fn asked_from(&self, vt: VtabId) -> String {
        let name = self.tree.vtab(vt).map(|v| v.name.as_str()).unwrap_or_default();
        format!("From workspace \u{201c}{name}\u{201d}: ")
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

    /// Serve the agent channel at `socket`: terminals opened from now on get
    /// it and their workspace's token in their environment.
    pub fn set_agent_socket(&mut self, socket: PathBuf) {
        self.agent_socket = Some(socket);
    }

    /// Give workspace `vt` a token, unless it has one or no socket is served.
    /// One that can't be made leaves the workspace without: its terminals get
    /// no agent environment.
    pub(super) fn mint_agent_token(&mut self, vt: VtabId) {
        if self.agent_socket.is_none() || self.agent_tokens.contains_key(&vt) {
            return;
        }
        match crate::agent::random_hex(32) {
            Ok(token) => {
                self.agent_tokens.insert(vt, token);
            }
            Err(e) => eprintln!("ghostrealm: no agent token for workspace {}: {e}", vt.0),
        }
    }

    /// The socket and workspace `vt`'s token, for a terminal's environment.
    pub(super) fn agent_env(&self, vt: Option<VtabId>) -> Option<(PathBuf, String)> {
        Some((self.agent_socket.clone()?, self.agent_tokens.get(&vt?)?.clone()))
    }

    /// Workspace `vt`'s token, if it has one.
    #[cfg(test)]
    pub(crate) fn agent_token(&self, vt: VtabId) -> Option<String> {
        self.agent_tokens.get(&vt).cloned()
    }

    /// The workspace `token` is the token of, while that workspace is open.
    /// Closing a workspace by any path revokes its token here; workspace ids
    /// aren't reused, so an old token never names a new one. Each token is
    /// compared in constant time.
    pub fn agent_token_workspace(&mut self, token: &str) -> Option<VtabId> {
        let tree = &self.tree;
        self.agent_tokens.retain(|vt, _| tree.vtab(*vt).is_some());
        self.agent_tokens.iter().filter(|(_, t)| same(t.as_bytes(), token.as_bytes())).map(|(vt, _)| *vt).next()
    }
}

/// Whether `a` and `b` are equal, in a time that depends only on their
/// lengths.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text of terminal `sid` once it shows `want`, waiting up to 10 s.
    fn shows(s: &mut AppState, sid: SurfaceId, want: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let text = s.surface_text(sid).unwrap_or_default();
            if want(&text) || Instant::now() >= deadline {
                return text;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Given an app serving the agent socket;
    /// when terminals open in two workspaces, and the first closes;
    /// then each terminal's environment holds the socket and its workspace's
    /// token, the tokens differ and each names its workspace, and the closed
    /// workspace's token names none.
    #[test]
    fn each_workspaces_terminals_get_its_token_until_it_closes() {
        let mut s = AppState::new().with_shell_line(r#"printf "<$GHOSTREALM_AGENT_SOCKET|$GHOSTREALM_AGENT_TOKEN>"; sleep 5"#);
        s.set_agent_socket(PathBuf::from("/tmp/agent.sock"));
        let mut seen = Vec::new();
        for _ in 0..2 {
            let vt = s.new_vtab().unwrap();
            let sid = s.focused_surface().unwrap();
            let text = shows(&mut s, sid, |t| t.contains('>'));
            // The line wraps at the terminal's width.
            let inner = text.split_once("</tmp/agent.sock|").and_then(|(_, t)| t.split_once('>')).map(|(t, _)| t.replace('\n', ""));
            let token = inner.unwrap_or_else(|| panic!("socket and token in the environment: {text}"));
            assert_eq!(token.len(), 64, "{token}");
            assert_eq!(s.agent_token_workspace(&token), Some(vt));
            seen.push((vt, token));
        }
        assert_ne!(seen[0].1, seen[1].1);
        s.close_vtab(seen[0].0);
        assert_eq!(s.agent_token_workspace(&seen[0].1), None, "revoked with its workspace");
        assert_eq!(s.agent_token_workspace(&seen[1].1), Some(seen[1].0));
        assert_eq!(s.agent_token_workspace(""), None);
    }
}
