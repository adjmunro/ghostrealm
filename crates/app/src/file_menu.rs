//! The file and folder context menu's model: what it is opened on, the actions
//! it offers, and which of them each target gets. Painting and clicking live
//! in the window; running an action is [`AppState`](crate::app_state::AppState)'s.

use std::path::PathBuf;

use ghostrealm_core::{Args, Value};

/// Where the menu was opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A row (or empty space) in a file browser.
    Browser,
    /// A document's header (its file name, `⋯`, or a right-click on it).
    Document,
}

/// What the menu was opened on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// Empty space in a file browser: the directory it shows.
    Empty,
}

/// The file, folder, or empty space a menu acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileTarget {
    /// The entry, or for [`Kind::Empty`] the directory being shown.
    pub path: PathBuf,
    pub kind: Kind,
    pub origin: Origin,
}

impl FileTarget {
    /// The directory new entries and pastes go into: the directory itself, or
    /// a file's folder.
    pub fn container(&self) -> PathBuf {
        match self.kind {
            Kind::File => self.path.parent().map(PathBuf::from).unwrap_or_default(),
            Kind::Dir | Kind::Empty => self.path.clone(),
        }
    }
}

/// One thing the menu can do. Each is also a command (see [`Self::command`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileAction {
    NewFile,
    NewFolder,
    Reveal,
    ShowInBrowser,
    OpenDefault,
    OpenInTerminal,
    Cut,
    Copy,
    Paste,
    Duplicate,
    CopyPath,
    CopyRelativePath,
    AddToGitignore,
    AddToExclude,
    Rename,
    SaveCopyAs,
    Delete,
}

impl FileAction {
    pub fn label(self) -> &'static str {
        match self {
            FileAction::NewFile => "New File\u{2026}",
            FileAction::NewFolder => "New Folder\u{2026}",
            FileAction::Reveal => "Reveal in Finder",
            FileAction::ShowInBrowser => "Show in File Browser",
            FileAction::OpenDefault => "Open in Default App",
            FileAction::OpenInTerminal => "Open in Terminal",
            FileAction::Cut => "Cut",
            FileAction::Copy => "Copy",
            FileAction::Paste => "Paste",
            FileAction::Duplicate => "Duplicate",
            FileAction::CopyPath => "Copy Path",
            FileAction::CopyRelativePath => "Copy Relative Path",
            FileAction::AddToGitignore => "Add to .gitignore",
            FileAction::AddToExclude => "Add to .git/info/exclude",
            FileAction::Rename => "Rename\u{2026}",
            FileAction::SaveCopyAs => "Save Copy As\u{2026}",
            FileAction::Delete => "Delete",
        }
    }

    /// The command's name in the palette.
    pub fn title(self) -> &'static str {
        match self {
            FileAction::NewFile => "New File in Folder",
            FileAction::NewFolder => "New Folder",
            FileAction::Reveal => "Reveal in Finder",
            FileAction::ShowInBrowser => "Show in File Browser",
            FileAction::OpenDefault => "Open in Default App",
            FileAction::OpenInTerminal => "Open in Terminal",
            FileAction::Cut => "Cut File",
            FileAction::Copy => "Copy File",
            FileAction::Paste => "Paste File",
            FileAction::Duplicate => "Duplicate File",
            FileAction::CopyPath => "Copy Path",
            FileAction::CopyRelativePath => "Copy Relative Path",
            FileAction::AddToGitignore => "Add to .gitignore",
            FileAction::AddToExclude => "Add to .git/info/exclude",
            FileAction::Rename => "Rename File",
            FileAction::SaveCopyAs => "Save Copy As",
            FileAction::Delete => "Delete File",
        }
    }

    /// The command's arguments for running on `target`: the folder for the
    /// actions that create or paste into one, else the path.
    pub fn args(self, target: &FileTarget) -> Args {
        let text = |p: PathBuf| Value::Str(p.to_string_lossy().into_owned());
        match self {
            FileAction::NewFile | FileAction::NewFolder | FileAction::Paste => {
                Args::new().with("dir", text(target.container()))
            }
            _ => Args::new().with("path", text(target.path.clone())),
        }
    }

    /// The registry command that runs this action.
    pub fn command(self) -> &'static str {
        match self {
            FileAction::NewFile => "file.new_file",
            FileAction::NewFolder => "file.new_folder",
            FileAction::Reveal => "file.reveal",
            FileAction::ShowInBrowser => "file.show_in_browser",
            FileAction::OpenDefault => "file.open_default",
            FileAction::OpenInTerminal => "file.open_in_terminal",
            FileAction::Cut => "file.cut",
            FileAction::Copy => "file.copy",
            FileAction::Paste => "file.paste",
            FileAction::Duplicate => "file.duplicate",
            FileAction::CopyPath => "file.copy_path",
            FileAction::CopyRelativePath => "file.copy_relative_path",
            FileAction::AddToGitignore => "file.add_to_gitignore",
            FileAction::AddToExclude => "file.add_to_git_exclude",
            FileAction::Rename => "file.rename",
            FileAction::SaveCopyAs => "file.save_copy_as",
            FileAction::Delete => "file.delete",
        }
    }
}

/// What the menu depends on besides its target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MenuEnv {
    /// The target is inside a git repository.
    pub in_git: bool,
    /// Something was cut or copied.
    pub can_paste: bool,
    /// The system has a "reveal in file manager" (macOS).
    pub reveal: bool,
}

/// A row of the menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuEntry {
    Item(FileAction),
    /// A horizontal rule between sections.
    Rule,
}

/// The menu for `target`: sections separated by rules, Delete last.
pub fn entries(target: &FileTarget, env: MenuEnv) -> Vec<MenuEntry> {
    use FileAction::*;
    let (kind, doc) = (target.kind, target.origin == Origin::Document);
    let entry = kind != Kind::Empty;
    let dir = kind != Kind::File;
    let mut sections: Vec<Vec<FileAction>> = vec![
        [NewFile, NewFolder].into_iter().filter(|_| dir).collect(),
        [
            env.reveal.then_some(Reveal),
            doc.then_some(ShowInBrowser),
            Some(OpenDefault),
            dir.then_some(OpenInTerminal),
        ]
        .into_iter()
        .flatten()
        .collect(),
        [
            entry.then_some(Cut),
            entry.then_some(Copy),
            env.can_paste.then_some(Paste),
            entry.then_some(Duplicate),
        ]
        .into_iter()
        .flatten()
        .collect(),
        [CopyPath, CopyRelativePath].into_iter().filter(|_| entry).collect(),
        [AddToGitignore, AddToExclude].into_iter().filter(|_| entry && env.in_git).collect(),
        [entry.then_some(Rename), doc.then_some(SaveCopyAs)].into_iter().flatten().collect(),
        [Delete].into_iter().filter(|_| entry).collect(),
    ];
    sections.retain(|s| !s.is_empty());
    let mut out = Vec::new();
    for (i, section) in sections.into_iter().enumerate() {
        if i > 0 {
            out.push(MenuEntry::Rule);
        }
        out.extend(section.into_iter().map(MenuEntry::Item));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use FileAction::*;

    fn target(kind: Kind, origin: Origin) -> FileTarget {
        FileTarget { path: PathBuf::from("/w/x"), kind, origin }
    }

    const ENV: MenuEnv = MenuEnv { in_git: true, can_paste: true, reveal: true };

    fn shape(t: FileTarget, env: MenuEnv) -> Vec<Option<FileAction>> {
        entries(&t, env)
            .into_iter()
            .map(|e| match e {
                MenuEntry::Item(a) => Some(a),
                MenuEntry::Rule => None,
            })
            .collect()
    }

    #[test]
    fn a_file_in_the_browser_gets_no_new_entries_and_no_terminal() {
        assert_eq!(
            shape(target(Kind::File, Origin::Browser), ENV),
            [
                Some(Reveal),
                Some(OpenDefault),
                None,
                Some(Cut),
                Some(Copy),
                Some(Paste),
                Some(Duplicate),
                None,
                Some(CopyPath),
                Some(CopyRelativePath),
                None,
                Some(AddToGitignore),
                Some(AddToExclude),
                None,
                Some(Rename),
                None,
                Some(Delete),
            ]
        );
    }

    #[test]
    fn a_directory_adds_new_entries_and_open_in_terminal() {
        let items = shape(target(Kind::Dir, Origin::Browser), ENV);
        assert_eq!(&items[..4], [Some(NewFile), Some(NewFolder), None, Some(Reveal)]);
        assert!(items.contains(&Some(OpenInTerminal)));
        assert_eq!(items.last(), Some(&Some(Delete)), "Delete is last");
        assert_eq!(items[items.len() - 2], None, "after a rule");
    }

    #[test]
    fn empty_space_offers_only_what_acts_on_the_folder_shown() {
        assert_eq!(
            shape(target(Kind::Empty, Origin::Browser), ENV),
            [
                Some(NewFile),
                Some(NewFolder),
                None,
                Some(Reveal),
                Some(OpenDefault),
                Some(OpenInTerminal),
                None,
                Some(Paste),
            ]
        );
        // With nothing to paste (and no Finder) it shrinks further.
        let env = MenuEnv { can_paste: false, reveal: false, ..ENV };
        assert_eq!(
            shape(target(Kind::Empty, Origin::Browser), env),
            [Some(NewFile), Some(NewFolder), None, Some(OpenDefault), Some(OpenInTerminal)]
        );
    }

    #[test]
    fn the_editors_file_name_gets_show_in_browser_and_save_copy_as() {
        let items = shape(target(Kind::File, Origin::Document), ENV);
        assert!(items.contains(&Some(ShowInBrowser)));
        assert!(items.contains(&Some(SaveCopyAs)));
        assert!(!items.contains(&Some(NewFile)));
        assert!(!items.contains(&Some(OpenInTerminal)));
        let pos = |a| items.iter().position(|i| *i == Some(a)).unwrap();
        assert!(pos(Rename) < pos(SaveCopyAs) && pos(SaveCopyAs) < pos(Delete));
        // The browser's file menu has neither.
        let items = shape(target(Kind::File, Origin::Browser), ENV);
        assert!(!items.contains(&Some(ShowInBrowser)) && !items.contains(&Some(SaveCopyAs)));
    }

    #[test]
    fn ignore_items_need_a_git_repository_and_vanish_with_their_section() {
        let env = MenuEnv { in_git: false, ..ENV };
        let items = shape(target(Kind::File, Origin::Browser), env);
        assert!(!items.contains(&Some(AddToGitignore)) && !items.contains(&Some(AddToExclude)));
        assert!(
            !items.windows(2).any(|w| w == [None, None]),
            "no empty section leaves two rules in a row"
        );
    }

    #[test]
    fn every_action_has_a_distinct_command() {
        let all = [
            NewFile, NewFolder, Reveal, ShowInBrowser, OpenDefault, OpenInTerminal, Cut, Copy, Paste, Duplicate,
            CopyPath, CopyRelativePath, AddToGitignore, AddToExclude, Rename, SaveCopyAs, Delete,
        ];
        let mut ids: Vec<_> = all.iter().map(|a| a.command()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), all.len());
    }

    #[test]
    fn new_entries_and_pastes_go_into_the_directory_or_a_files_folder() {
        assert_eq!(target(Kind::File, Origin::Browser).container(), PathBuf::from("/w"));
        assert_eq!(target(Kind::Dir, Origin::Browser).container(), PathBuf::from("/w/x"));
        assert_eq!(target(Kind::Empty, Origin::Browser).container(), PathBuf::from("/w/x"));
    }
}
