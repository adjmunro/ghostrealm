//! The sidebar's order, inbox style: pinned workspaces, then the rest, each
//! list newest [`Vtab::attention`] first. Workspaces of one list sharing a
//! root directory sit under that directory's header, placed by their newest
//! member. Viewing a workspace never moves it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::tree::{TabStatus, Tree, Vtab, VtabId};

/// One line of the sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// The header of the workspaces in list `pinned` whose root is `dir`.
    /// `status` is the most urgent of theirs.
    Group {
        pinned: bool,
        dir: PathBuf,
        collapsed: bool,
        status: TabStatus,
        /// A member, by which a click on the header names the group.
        lead: VtabId,
    },
    /// A workspace; `grouped` when it sits under a header.
    Vtab { id: VtabId, grouped: bool },
}

/// The attention stamps the sidebar was last ordered by, so the order can
/// hold while the pointer is over it.
#[derive(Clone, Debug, Default)]
pub struct Order {
    keys: HashMap<VtabId, u64>,
}

impl Order {
    /// The sidebar's rows. With `hold`, workspaces sort by the stamps of the
    /// last call (a workspace new since then by its own), so a request for
    /// attention moves nothing; without it, by their current stamps.
    /// `collapsed(pinned, dir)` says whether a group's members are hidden.
    pub fn rows(&mut self, tree: &Tree, hold: bool, collapsed: &dyn Fn(bool, &Path) -> bool) -> Vec<Row> {
        self.keys.retain(|id, _| tree.vtab(*id).is_some());
        for v in tree.vtabs() {
            if !hold || !self.keys.contains_key(&v.id) {
                self.keys.insert(v.id, v.attention);
            }
        }
        let key = |v: &Vtab| self.keys[&v.id];
        let mut rows = Vec::new();
        for pinned in [true, false] {
            let list: Vec<&Vtab> = tree.vtabs().iter().filter(|v| v.pinned == pinned).collect();
            let shares = |dir: &Path| list.iter().filter(|v| v.root_dir.as_deref() == Some(dir)).count() > 1;
            // (key, members, group dir), in tree order before sorting.
            let mut entries: Vec<(u64, Vec<&Vtab>, Option<&Path>)> = Vec::new();
            for v in &list {
                match v.root_dir.as_deref().filter(|d| shares(d)) {
                    Some(dir) => match entries.iter_mut().find(|e| e.2 == Some(dir)) {
                        Some(e) => e.1.push(v),
                        None => entries.push((0, vec![v], Some(dir))),
                    },
                    None => entries.push((0, vec![v], None)),
                }
            }
            for e in &mut entries {
                e.1.sort_by_key(|v| std::cmp::Reverse(key(v)));
                e.0 = key(e.1[0]);
            }
            entries.sort_by_key(|e| std::cmp::Reverse(e.0));
            for (_, members, dir) in entries {
                let Some(dir) = dir else {
                    rows.push(Row::Vtab { id: members[0].id, grouped: false });
                    continue;
                };
                let folded = collapsed(pinned, dir);
                rows.push(Row::Group {
                    pinned,
                    dir: dir.to_path_buf(),
                    collapsed: folded,
                    status: members.iter().map(|v| v.status).max_by_key(|s| urgency(*s)).unwrap_or(TabStatus::Read),
                    lead: members[0].id,
                });
                if !folded {
                    rows.extend(members.iter().map(|v| Row::Vtab { id: v.id, grouped: true }));
                }
            }
        }
        rows
    }
}

/// How strongly a status wants the user: a group's header shows its members' most urgent.
fn urgency(s: TabStatus) -> u8 {
    match s {
        TabStatus::Read => 0,
        TabStatus::Busy => 1,
        TabStatus::Unread { success: true } => 2,
        TabStatus::Unread { success: false } => 3,
        TabStatus::NeedsInput => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> &'static dyn Fn(bool, &Path) -> bool {
        &|_, _| false
    }

    /// The workspace ids of `rows`, with each group header as `0`.
    fn ids(rows: &[Row]) -> Vec<u64> {
        rows.iter()
            .map(|r| match r {
                Row::Group { .. } => 0,
                Row::Vtab { id, .. } => id.0,
            })
            .collect()
    }

    fn tree(n: usize) -> (Tree, Vec<VtabId>) {
        let mut t = Tree::new();
        let ids = (0..n).map(|i| t.add_empty_vtab(format!("w{i}")).0).collect();
        (t, ids)
    }

    fn root(t: &mut Tree, vt: VtabId, dir: &str) {
        t.vtab_mut(vt).unwrap().root_dir = Some(PathBuf::from(dir));
    }

    #[test]
    fn newest_attention_first_and_viewing_does_not_move_it_back() {
        let (mut t, w) = tree(3);
        let mut o = Order::default();
        assert_eq!(ids(&o.rows(&t, false, open())), [w[2].0, w[1].0, w[0].0], "newest created first");
        t.set_status(w[0], TabStatus::Unread { success: true });
        assert_eq!(ids(&o.rows(&t, false, open())), [w[0].0, w[2].0, w[1].0]);
        t.set_status(w[1], TabStatus::NeedsInput);
        assert_eq!(ids(&o.rows(&t, false, open())), [w[1].0, w[0].0, w[2].0]);
        t.set_status(w[1], TabStatus::Read);
        t.set_status(w[0], TabStatus::Busy);
        assert_eq!(ids(&o.rows(&t, false, open())), [w[1].0, w[0].0, w[2].0], "read and busy keep their place");
    }

    #[test]
    fn a_repeat_of_the_same_status_is_not_a_new_request() {
        let (mut t, w) = tree(2);
        t.set_status(w[0], TabStatus::Unread { success: true });
        t.set_status(w[1], TabStatus::Unread { success: true });
        let stamp = t.vtab(w[0]).unwrap().attention;
        t.set_status(w[0], TabStatus::Unread { success: true });
        assert_eq!(t.vtab(w[0]).unwrap().attention, stamp, "more output while unread");
        t.set_status(w[0], TabStatus::NeedsInput);
        assert!(t.vtab(w[0]).unwrap().attention > t.vtab(w[1]).unwrap().attention, "a new kind of request");
    }

    #[test]
    fn pinned_workspaces_list_first_each_list_by_attention() {
        let (mut t, w) = tree(4);
        t.vtab_mut(w[0]).unwrap().pinned = true;
        t.vtab_mut(w[1]).unwrap().pinned = true;
        let mut o = Order::default();
        assert_eq!(ids(&o.rows(&t, false, open())), [w[1].0, w[0].0, w[3].0, w[2].0]);
        t.set_status(w[2], TabStatus::NeedsInput);
        t.set_status(w[0], TabStatus::Unread { success: true });
        assert_eq!(
            ids(&o.rows(&t, false, open())),
            [w[0].0, w[1].0, w[2].0, w[3].0],
            "attention reorders within a list, never across"
        );
    }

    #[test]
    fn workspaces_sharing_a_root_group_and_the_group_rises_with_any_member() {
        let (mut t, w) = tree(4);
        root(&mut t, w[0], "/p");
        root(&mut t, w[2], "/p");
        root(&mut t, w[1], "/q");
        let mut o = Order::default();
        let rows = o.rows(&t, false, open());
        assert_eq!(ids(&rows), [w[3].0, 0, w[2].0, w[0].0, w[1].0], "a lone root is no group");
        assert_eq!(
            rows[1],
            Row::Group { pinned: false, dir: "/p".into(), collapsed: false, status: TabStatus::Read, lead: w[2] }
        );
        assert_eq!(rows[2], Row::Vtab { id: w[2], grouped: true });
        t.set_status(w[1], TabStatus::Unread { success: true });
        t.set_status(w[0], TabStatus::NeedsInput);
        let rows = o.rows(&t, false, open());
        assert_eq!(ids(&rows), [0, w[0].0, w[2].0, w[1].0, w[3].0]);
        assert!(matches!(rows[0], Row::Group { status: TabStatus::NeedsInput, lead, .. } if lead == w[0]));
    }

    #[test]
    fn pinned_and_unpinned_members_of_one_root_group_apart() {
        let (mut t, w) = tree(4);
        for &vt in &w {
            root(&mut t, vt, "/p");
        }
        t.vtab_mut(w[0]).unwrap().pinned = true;
        t.vtab_mut(w[1]).unwrap().pinned = true;
        let rows = Order::default().rows(&t, false, open());
        assert_eq!(ids(&rows), [0, w[1].0, w[0].0, 0, w[3].0, w[2].0]);
        assert!(matches!(rows[0], Row::Group { pinned: true, .. }));
        assert!(matches!(rows[3], Row::Group { pinned: false, .. }));
    }

    #[test]
    fn a_collapsed_group_hides_its_members() {
        let (mut t, w) = tree(3);
        root(&mut t, w[0], "/p");
        root(&mut t, w[1], "/p");
        let rows = Order::default().rows(&t, false, &|pinned, d| !pinned && d == Path::new("/p"));
        assert_eq!(ids(&rows), [w[2].0, 0]);
        assert!(matches!(rows[1], Row::Group { collapsed: true, .. }));
    }

    #[test]
    fn holding_keeps_the_order_and_letting_go_resorts() {
        let (mut t, w) = tree(3);
        root(&mut t, w[0], "/p");
        root(&mut t, w[1], "/p");
        let mut o = Order::default();
        let before = o.rows(&t, false, open());
        assert_eq!(ids(&before), [w[2].0, 0, w[1].0, w[0].0]);
        let (vt, _) = t.add_empty_vtab("new");
        let held = o.rows(&t, true, open());
        assert_eq!(ids(&held), [vt.0, w[2].0, 0, w[1].0, w[0].0], "a new workspace still shows");
        t.set_status(w[0], TabStatus::NeedsInput);
        let held = o.rows(&t, true, open());
        assert_eq!(ids(&held), [vt.0, w[2].0, 0, w[1].0, w[0].0], "a request moves nothing while held");
        assert!(
            matches!(held[2], Row::Group { status: TabStatus::NeedsInput, .. }),
            "statuses still show live"
        );
        t.close_vtab(w[2]);
        assert_eq!(ids(&o.rows(&t, true, open())), [vt.0, 0, w[1].0, w[0].0]);
        assert_eq!(ids(&o.rows(&t, false, open())), [0, w[0].0, w[1].0, vt.0], "let go: re-sorted");
    }
}
