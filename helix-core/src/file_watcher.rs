use std::borrow::Borrow;
use std::collections::{BTreeSet, HashSet};
use std::iter;
use std::mem::replace;
use std::path::{Path, PathBuf};
use std::slice;
use std::sync::Arc;
use std::time::SystemTime;

// Re-export filesentry types (available on all platforms)
pub use filesentry::{CanonicalPathBuf, Event, EventType, Events, Filter, ShutdownOnDrop};

use helix_event::{dispatch, events};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::{Deserialize, Serialize};

events! {
    FileSystemDidChange {
        fs_events: Events
    }
    // The watcher finished its first crawl of a root, or failed to watch it.
    // What was on disk at the crawl is the watcher's starting state, so a
    // change made before it is never reported.
    WatchRootCrawled {}
}

/// Create an Events collection from an iterator of paths.
/// All paths will have EventType::Modified.
pub fn events_from_paths(paths: impl IntoIterator<Item = PathBuf>) -> Events {
    use filesentry::CanonicalPathBuf;
    let events: Vec<Event> = paths
        .into_iter()
        .filter_map(|path| {
            let canonical = path.canonicalize().ok()?;
            Some(Event {
                path: CanonicalPathBuf::assert_canonicalized(&canonical),
                ty: EventType::Modified,
            })
        })
        .collect();
    Events::from(events)
}

/// Config for file watching
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct Config {
    /// Enable file watching enable by default
    pub enable: bool,
    pub watch_vcs: bool,
    /// Only enable the file watcher inside helix workspaces (VCS repos and directories with .helix
    /// directory) this prevents watching large directories like $HOME by default
    ///
    /// Defaults to `true`
    pub require_workspace: bool,
    /// Ignore hidden files and directories, other than .helix, .github, .cargo
    /// and .envrc, as the file picker's `hidden` does. Defaults to false, so
    /// dotfiles like .eslintrc stay watched.
    pub hidden: bool,
    /// Enables reading `.ignore` files.
    /// Whether to hide files listed in .ignore in file picker and global search results. Defaults to true.
    pub ignore: bool,
    /// Enables reading `.gitignore` files.
    /// Whether to hide files listed in .gitignore in file picker and global search results. Defaults to true.
    pub git_ignore: bool,
    /// Enables reading global .gitignore, whose path is specified in git's config: `core.excludefile` option.
    /// Whether to hide files listed in global .gitignore in file picker and global search results. Defaults to true.
    pub git_global: bool,
    // /// Enables reading `.git/info/exclude` files.
    // /// Whether to hide files listed in .git/info/exclude in file picker and global search results. Defaults to true.
    // pub git_exclude: bool,
    /// Also watch the files open in the editor that the workspace watch skips:
    /// outside the workspace, or in an ignored or hidden directory. Watches each
    /// one's directory and drops events for the other files in it. Applies
    /// outside a workspace too when `require-workspace` is set.
    ///
    /// Defaults to `true`
    pub watch_open_files: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enable: true,
            watch_vcs: true,
            require_workspace: true,
            hidden: false,
            ignore: true,
            git_ignore: true,
            git_global: true,
            watch_open_files: true,
        }
    }
}

pub struct Watcher {
    watcher: Option<(filesentry::Watcher, ShutdownOnDrop)>,
    filter: Arc<WatchFilter>,
    /// Roots language servers asked for
    roots: BTreeSet<PathBuf>,
    config: Config,
    /// Extra paths that need polling (e.g., VCS HEAD files outside workspace)
    /// Stored with their last known mtime for change detection
    extra_watched_paths: Vec<(PathBuf, Option<SystemTime>)>,
    /// Real paths of the files open in the editor
    open_files: BTreeSet<PathBuf>,
    /// Roots given to filesentry, and whether recursively: it can't remove one,
    /// and logs an error when one is added again
    added_roots: HashSet<(PathBuf, bool)>,
}

impl Watcher {
    pub fn new(config: &Config) -> Watcher {
        let mut watcher = Watcher {
            watcher: None,
            filter: Arc::new(WatchFilter {
                filesentry_ignores: Gitignore::empty(),
                ignore_files: Vec::new(),
                global_ignores: Vec::new(),
                hidden: false,
                watch_vcs: true,
                open_files: HashSet::new(),
                open_dirs: HashSet::new(),
            }),
            roots: BTreeSet::new(),
            config: config.clone(),
            extra_watched_paths: Vec::new(),
            open_files: BTreeSet::new(),
            added_roots: HashSet::new(),
        };
        watcher.reload(config);
        watcher
    }

    pub fn reload(&mut self, config: &Config) {
        let old_config = replace(&mut self.config, config.clone());
        let watch_roots = self.watch_roots();

        if !config.enable || !watch_roots && !config.watch_open_files {
            self.watcher = None;
            return;
        }
        self.filter = Arc::new(self.build_filter());
        match &self.watcher {
            Some((watcher, _)) => {
                // TODO: more fine grained detection of when recrawl is nedded
                watcher.set_filter(self.filter.clone(), old_config != self.config);
            }
            None => match filesentry::Watcher::new() {
                Ok(watcher) => {
                    watcher.set_filter(self.filter.clone(), false);
                    let dispatch = dispatcher();
                    watcher.add_handler(move |events| {
                        dispatch(FileSystemDidChange { fs_events: events });
                        true
                    });
                    watcher.start();
                    let shutdown_guard = watcher.shutdown_guard();
                    self.watcher = Some((watcher, shutdown_guard));
                    self.added_roots.clear();
                }
                Err(err) => {
                    log::info!("file-watcher not available: {err}");
                    return;
                }
            },
        }
        self.add_roots();
    }

    /// Gives filesentry the roots it doesn't have yet: the workspace and the
    /// roots added to it, and the directories of open files they don't cover.
    fn add_roots(&mut self) {
        let watch_roots = self.watch_roots();
        let Some((watcher, _)) = &self.watcher else {
            return;
        };
        let (workspace, _) = helix_loader::find_workspace();
        let roots = iter::once(&workspace)
            .chain(&self.roots)
            .filter(|_| watch_roots)
            .map(|root| (root, true));
        let open_dirs = self.filter.open_dirs.iter().map(|dir| (dir, false));
        for (root, recursive) in roots.chain(open_dirs) {
            if self.added_roots.insert((root.clone(), recursive)) {
                let dispatch = dispatcher();
                let crawled = move |_| dispatch(WatchRootCrawled {});
                if let Err(err) = watcher.add_root(root, recursive, crawled) {
                    log::error!("failed to watch {root:?}: {err}");
                }
            }
        }
    }

    /// Whether to watch the workspace and the roots added to it. Without a
    /// workspace, `require-workspace` leaves only the open files watched.
    fn watch_roots(&self) -> bool {
        !self.config.require_workspace || !helix_loader::find_workspace().1
    }

    fn build_filter(&self) -> WatchFilter {
        let (workspace, _) = helix_loader::find_workspace();
        let watch_roots = self.watch_roots();
        let roots = self.roots.iter().map(|root| &**root);
        let filter = WatchFilter::new(
            &self.config,
            &workspace,
            roots.chain([&*workspace]).filter(|_| watch_roots),
        );
        let open_files = self
            .open_files
            .iter()
            .filter(|_| self.config.watch_open_files);
        filter.with_open_files(open_files)
    }

    /// Sets the files open in the editor, to watch the ones the roots don't.
    pub fn set_open_files(&mut self, paths: impl IntoIterator<Item = impl AsRef<Path>>) {
        let open_files: BTreeSet<_> = paths
            .into_iter()
            .filter_map(|path| real_path(path.as_ref()))
            .collect();
        if open_files == self.open_files {
            return;
        }
        self.open_files = open_files;
        if let Some((watcher, _)) = &self.watcher {
            self.filter = Arc::new(self.build_filter());
            watcher.set_filter(self.filter.clone(), false);
            self.add_roots();
        }
    }

    /// Check if a given path is being actively watched: an open file, or under a
    /// watched root and not filtered out.
    pub fn is_watching(&self, path: &Path) -> bool {
        self.watcher.is_some()
            && path
                .canonicalize()
                .is_ok_and(|path| !self.filter.ignore_path_rec(&path, Some(false)))
    }

    /// Poll extra watched paths for changes and return paths that have changed.
    /// Updates internal mtime tracking for the changed paths.
    pub fn poll_extra_paths(&mut self) -> Vec<PathBuf> {
        let mut changed = Vec::new();
        for (path, last_mtime) in &mut self.extra_watched_paths {
            let current_mtime = path.metadata().ok().and_then(|m| m.modified().ok());
            if current_mtime != *last_mtime {
                changed.push(path.clone());
                *last_mtime = current_mtime;
            }
        }
        changed
    }

    /// Set extra paths to watch via polling.
    /// These are paths outside the main watched workspace that need change detection.
    /// Only paths outside the workspace are added (paths inside are already watched).
    pub fn set_extra_watched_paths(&mut self, paths: Vec<PathBuf>) {
        let (workspace, _) = helix_loader::find_workspace();
        self.extra_watched_paths = paths
            .into_iter()
            .filter(|path| !path.starts_with(&workspace))
            .map(|path| {
                let mtime = path.metadata().ok().and_then(|m| m.modified().ok());
                (path, mtime)
            })
            .collect();
        if !self.extra_watched_paths.is_empty() {
            log::info!(
                "added {} extra paths for polling: {:?}",
                self.extra_watched_paths.len(),
                self.extra_watched_paths
                    .iter()
                    .map(|(p, _)| p)
                    .collect::<Vec<_>>()
            );
        }
    }

    pub fn add_root(&mut self, root: &Path) {
        let root = match root.canonicalize() {
            Ok(root) => root,
            Err(err) => {
                log::error!("failed to watch {root:?}: {err}");
                return;
            }
        };
        // Already a root, or in one and not ignored there
        if self.roots.contains(&root) || self.filter.covers(&root, Some(true)) {
            return;
        }
        let (workspace, _) = helix_loader::find_workspace();
        if root.starts_with(&workspace) {
            return;
        }
        self.roots.insert(root);
        self.filter = Arc::new(self.build_filter());
        if let Some((watcher, _)) = &self.watcher {
            watcher.set_filter(self.filter.clone(), false);
        }
        self.add_roots();
    }
}

/// A `dispatch` for filesentry to call. It calls back on its own thread, outside
/// the tokio runtime. Hooks may call `send_blocking`, which needs a runtime for
/// its timeout when a channel is full, so this enters the runtime of the thread
/// that made it.
fn dispatcher<E: helix_event::Event>() -> impl Fn(E) + Send + 'static {
    let runtime = tokio::runtime::Handle::try_current().ok();
    move |event| {
        let _guard = runtime.as_ref().map(|runtime| runtime.enter());
        dispatch(event);
    }
}

/// The real path of a file, or for one not written yet, of its directory
/// joined with its name.
fn real_path(path: &Path) -> Option<PathBuf> {
    path.canonicalize()
        .ok()
        .or_else(|| Some(path.parent()?.canonicalize().ok()?.join(path.file_name()?)))
}

fn build_ignore(paths: impl IntoIterator<Item = PathBuf> + Clone, dir: &Path) -> Option<Gitignore> {
    let mut builder = GitignoreBuilder::new(dir);
    for path in paths.clone() {
        if let Some(err) = builder.add(&path) {
            if !err.is_io() {
                log::error!("failed to read ignorefile at {path:?}: {err}");
            }
        }
    }
    match builder.build() {
        Ok(ignore) => (!ignore.is_empty()).then_some(ignore),
        Err(err) => {
            if !err.is_io() {
                log::error!(
                    "failed to read ignorefile at {:?}: {err}",
                    paths.into_iter().collect::<Vec<_>>()
                );
            }
            None
        }
    }
}

struct IgnoreFiles {
    root: PathBuf,
    ignores: Vec<Arc<Gitignore>>,
}

impl IgnoreFiles {
    fn new(
        workspace_ignore: Option<Arc<Gitignore>>,
        config: &Config,
        root: &Path,
        globals: &[Arc<Gitignore>],
    ) -> Self {
        let mut ignores = Vec::with_capacity(8);
        // .helix/ignore
        if let Some(workspace_ignore) = workspace_ignore {
            ignores.push(workspace_ignore);
        }
        for ancestor in root.ancestors() {
            let ignore = if config.ignore {
                if config.git_ignore {
                    // the second path takes priority
                    build_ignore(
                        [ancestor.join(".gitignore"), ancestor.join(".ignore")],
                        ancestor,
                    )
                } else {
                    build_ignore([ancestor.join(".ignore")], ancestor)
                }
            } else if config.git_ignore {
                build_ignore([ancestor.join(".gitignore")], ancestor)
            } else {
                None
            };
            if let Some(ignore) = ignore {
                ignores.push(Arc::new(ignore));
            }
        }
        ignores.extend(globals.iter().cloned());
        Self {
            root: root.into(),
            ignores,
        }
    }

    fn shared_ignores(
        workspace: &Path,
        config: &Config,
    ) -> (Vec<Arc<Gitignore>>, Option<Arc<Gitignore>>) {
        let mut ignores = Vec::new();
        let workspace_ignore = build_ignore(
            [
                helix_loader::config_dir().join("ignore"),
                workspace.join(".helix/ignore"),
            ],
            workspace,
        )
        .map(Arc::new);
        if config.git_global {
            let (gitignore_global, err) = Gitignore::global();
            if let Some(err) = err {
                if !err.is_io() {
                    log::error!("failed to read global global ignorefile: {err}");
                }
            }
            if !gitignore_global.is_empty() {
                ignores.push(Arc::new(gitignore_global));
            }
        }
        // if config.git_exclude {
        // TODO git_exclude implementation, this isn't quite trivial unfortunaetly
        // due to detached workspace etc.
        // }
        // TODO: git exclude
        (ignores, workspace_ignore)
    }

    fn filesentry_ignores(workspace: &Path) -> Gitignore {
        // the second path takes priority
        build_ignore(
            [
                helix_loader::config_dir().join("filesentryignore"),
                workspace.join(".helix/filesentryignore"),
            ],
            workspace,
        )
        .unwrap_or(Gitignore::empty())
    }

    fn is_ignored(
        ignores: &[impl Borrow<Gitignore>],
        path: &Path,
        is_dir: Option<bool>,
    ) -> Option<bool> {
        match is_dir {
            Some(is_dir) => {
                for ignore in ignores {
                    match ignore.borrow().matched(path, is_dir) {
                        ignore::Match::None => continue,
                        ignore::Match::Ignore(_) => return Some(true),
                        ignore::Match::Whitelist(_) => return Some(false),
                    }
                }
            }
            None => {
                // if we don't know wether this is a directory (on windows)
                // then we are conservative and allow the dirs
                for ignore in ignores {
                    match ignore.borrow().matched(path, true) {
                        ignore::Match::None => continue,
                        ignore::Match::Ignore(glob) => {
                            if glob.is_only_dir() {
                                match ignore.borrow().matched(path, false) {
                                    ignore::Match::None => continue,
                                    ignore::Match::Ignore(_) => return Some(true),
                                    ignore::Match::Whitelist(_) => return Some(false),
                                }
                            } else {
                                return Some(true);
                            }
                        }
                        ignore::Match::Whitelist(_) => return Some(false),
                    }
                }
            }
        }
        None
    }
}

/// a filter to ignore hiddeng/ingored files. The point of this
/// is to avoid overwhelming the watcher with watching a ton of
/// files/directories (like the cargo target directory, node_modules or
/// VCS files) so ignoring a file is a performance optimization.
///
/// By default we ignore ignored
struct WatchFilter {
    filesentry_ignores: Gitignore,
    ignore_files: Vec<IgnoreFiles>,
    global_ignores: Vec<Arc<Gitignore>>,
    hidden: bool,
    watch_vcs: bool,
    /// Open files the roots don't cover
    open_files: HashSet<PathBuf>,
    /// Their directories, where the roots don't cover those either. Each is
    /// watched without recursing, and lets through only the open files in it.
    open_dirs: HashSet<PathBuf>,
}

impl WatchFilter {
    fn new<'a>(
        config: &Config,
        workspace: &'a Path,
        roots: impl Iterator<Item = &'a Path> + Clone,
    ) -> WatchFilter {
        let filesentry_ignores = IgnoreFiles::filesentry_ignores(workspace);
        let (global_ignores, workspace_ignore) = IgnoreFiles::shared_ignores(workspace, config);
        let ignore_files = roots
            .map(|root| IgnoreFiles::new(workspace_ignore.clone(), config, root, &global_ignores))
            .collect();
        WatchFilter {
            filesentry_ignores,
            ignore_files,
            global_ignores,
            hidden: config.hidden,
            watch_vcs: config.watch_vcs,
            open_files: HashSet::new(),
            open_dirs: HashSet::new(),
        }
    }

    fn with_open_files<'a>(mut self, files: impl Iterator<Item = &'a PathBuf>) -> Self {
        for file in files {
            if self.covers(file, Some(false)) {
                continue;
            }
            if let Some(dir) = file.parent().filter(|dir| !self.covers(dir, Some(true))) {
                self.open_dirs.insert(dir.to_owned());
            }
            self.open_files.insert(file.clone());
        }
        self
    }

    fn is_open(&self, path: &Path) -> bool {
        self.open_files.contains(path) || self.open_dirs.contains(path)
    }

    /// Whether the root watches cover `path`: it's under a root and not ignored.
    fn covers(&self, mut path: &Path, mut is_dir: Option<bool>) -> bool {
        let Some((root, ignore_files)) = self.ignore_files_for(path) else {
            return false;
        };
        loop {
            if path == root {
                return true;
            }
            if self.ignore_path_impl(path, is_dir, ignore_files) {
                return false;
            }
            let Some(parent) = path.parent() else {
                break;
            };
            path = parent;
            // Ancestors of the leaf are always directories. Passing the leaf's
            // `is_dir` up the chain makes a dir-only pattern (gitignore `target/`)
            // miss the ancestor directory it names.
            is_dir = Some(true);
        }
        true
    }

    fn ignore_path_impl(
        &self,
        path: &Path,
        is_dir: Option<bool>,
        ignore_files: &[Arc<Gitignore>],
    ) -> bool {
        if let Some(ignore) =
            IgnoreFiles::is_ignored(slice::from_ref(&self.filesentry_ignores), path, is_dir)
        {
            return ignore;
        }
        if is_hardcoded_whitelist(path) {
            return false;
        }
        if is_hardcoded_blacklist(path, is_dir.unwrap_or(false)) {
            return true;
        }
        if let Some(ignore) = IgnoreFiles::is_ignored(ignore_files, path, is_dir) {
            return ignore;
        }
        // ignore .git dircectory except .git/HEAD (and .git itself)
        if is_vcs_ignore(path, self.watch_vcs) {
            return true;
        }
        self.hidden && is_hidden(path)
    }

    /// The nearest root `path` is under, and its ignore files
    fn ignore_files_for(&self, path: &Path) -> Option<(&Path, &[Arc<Gitignore>])> {
        self.ignore_files
            .iter()
            .filter(|files| path.starts_with(&files.root))
            .max_by_key(|files| files.root.components().count())
            .map(|files| (files.root.as_path(), files.ignores.as_slice()))
    }
}

impl filesentry::Filter for WatchFilter {
    fn ignore_path(&self, path: &Path, is_dir: Option<bool>) -> bool {
        if self.is_open(path) {
            return false;
        }
        if path
            .parent()
            .is_some_and(|dir| self.open_dirs.contains(dir))
        {
            return !self.covers(path, is_dir);
        }
        let (root, ignore_files) = self
            .ignore_files_for(path)
            .unwrap_or((Path::new(""), &self.global_ignores));
        if path == root {
            return false;
        }
        self.ignore_path_impl(path, is_dir, ignore_files)
    }

    fn ignore_path_rec(&self, path: &Path, is_dir: Option<bool>) -> bool {
        !self.is_open(path) && !self.covers(path, is_dir)
    }
}

fn is_hidden(path: &Path) -> bool {
    path.file_name().is_some_and(|it| {
        it.as_encoded_bytes().first() == Some(&b'.')
        // handled by vcs ignore rules
        && it != ".git"
    })
}

// hidden directories we want to watch by default
fn is_hardcoded_whitelist(path: &Path) -> bool {
    path.ends_with(".helix")
        | path.ends_with(".github")
        | path.ends_with(".cargo")
        | path.ends_with(".envrc")
}

fn is_hardcoded_blacklist(path: &Path, is_dir: bool) -> bool {
    // don't descend into the cargo regstiry and similar
    path.parent()
        .is_some_and(|parent| parent.ends_with(".cargo"))
        && is_dir
}

fn file_name(path: &Path) -> Option<&str> {
    path.file_name().and_then(|it| it.to_str())
}

fn is_vcs_ignore(path: &Path, watch_vcs: bool) -> bool {
    // ignore .git directory contents except .git/HEAD (and .git itself)
    // Note: only checks immediate parent; recursive checking is done by ignore_path_rec
    if watch_vcs
        && path.parent().is_some_and(|it| it.ends_with(".git"))
        && !path.ends_with(".git/HEAD")
    {
        return true;
    }
    match file_name(path) {
        Some(".jj" | ".svn" | ".hg") => true,
        Some(".git") => !watch_vcs,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::Path;

    use crate::file_watcher::{is_hardcoded_whitelist, is_hidden, is_vcs_ignore};

    #[test]
    fn test_vcs_ignore() {
        assert!(!is_vcs_ignore(Path::new(".git"), true));
        assert!(!is_vcs_ignore(Path::new(".git/HEAD"), true));
        assert!(is_vcs_ignore(Path::new(".git/foo"), true));
        // Note: .git/foo/bar is NOT caught by is_vcs_ignore (only checks immediate parent)
        // but it IS caught by ignore_path_rec which checks ancestors recursively
        assert!(!is_vcs_ignore(Path::new(".git/foo/bar"), true));
        assert!(!is_vcs_ignore(Path::new(".foo"), true));
        assert!(is_vcs_ignore(Path::new(".jj"), true));
        assert!(is_vcs_ignore(Path::new(".svn"), true));
        assert!(is_vcs_ignore(Path::new(".hg"), true));
    }

    #[test]
    fn test_hidden() {
        assert!(is_hidden(Path::new(".foo")));
        // handled by vcs ignore rules
        assert!(!is_hidden(Path::new(".git")));
    }

    #[test]
    fn test_whitelist() {
        // Note: .git is NOT in whitelist - it has special handling in is_vcs_ignore and is_hidden
        assert!(is_hardcoded_whitelist(Path::new(".helix")));
        assert!(is_hardcoded_whitelist(Path::new(".github")));
        assert!(!is_hardcoded_whitelist(Path::new(".githup")));
    }

    #[test]
    fn ignore_path_rec_treats_ancestors_as_dirs() {
        use std::sync::Arc;

        use filesentry::Filter;
        use ignore::gitignore::{Gitignore, GitignoreBuilder};

        use crate::file_watcher::{IgnoreFiles, WatchFilter};

        let mut builder = GitignoreBuilder::new("/repo");
        // dir-only pattern: matches the `target` directory, not a file named target
        builder.add_line(None, "target/").unwrap();
        let ignores = builder.build().unwrap();
        let filter = WatchFilter {
            filesentry_ignores: Gitignore::empty(),
            ignore_files: vec![IgnoreFiles {
                root: "/repo".into(),
                ignores: vec![Arc::new(ignores)],
            }],
            global_ignores: Vec::new(),
            hidden: true,
            watch_vcs: true,
            open_files: HashSet::new(),
            open_dirs: HashSet::new(),
        };

        // A file under `target/` is ignored even though the leaf is passed as a file:
        // the walk must re-check the `target` ancestor as a directory.
        assert!(filter.ignore_path_rec(Path::new("/repo/target/foo.rs"), Some(false)));
        assert!(filter.ignore_path_rec(Path::new("/repo/target/deep/foo.rs"), Some(false)));
        assert!(!filter.ignore_path_rec(Path::new("/repo/src/main.rs"), Some(false)));
    }

    #[test]
    fn watches_open_files_the_roots_skip() {
        use std::path::PathBuf;
        use std::sync::Arc;

        use filesentry::Filter;
        use ignore::gitignore::{Gitignore, GitignoreBuilder};

        use crate::file_watcher::{IgnoreFiles, WatchFilter};

        let mut builder = GitignoreBuilder::new("/repo");
        builder.add_line(None, "notes/").unwrap();
        builder.add_line(None, "*.log").unwrap();
        let filter = WatchFilter {
            filesentry_ignores: Gitignore::empty(),
            ignore_files: vec![IgnoreFiles {
                root: "/repo".into(),
                ignores: vec![Arc::new(builder.build().unwrap())],
            }],
            global_ignores: Vec::new(),
            hidden: true,
            watch_vcs: true,
            open_files: HashSet::new(),
            open_dirs: HashSet::new(),
        };
        let open: Vec<PathBuf> = [
            "/repo/notes/a.md",
            "/repo/notes/b.md",
            "/repo/src/main.rs",
            "/repo/src/debug.log",
            "/elsewhere/c.md",
        ]
        .map(PathBuf::from)
        .into();
        let filter = filter.with_open_files(open.iter());
        let ignored = |path: &str, is_dir| filter.ignore_path(Path::new(path), Some(is_dir));
        let ignored_rec = |path: &str| filter.ignore_path_rec(Path::new(path), Some(false));

        // Only the directories the roots skip get a watch of their own.
        let dirs: HashSet<PathBuf> = ["/repo/notes", "/elsewhere"].map(PathBuf::from).into();
        assert_eq!(filter.open_dirs, dirs);
        assert!(!ignored("/repo/notes", true));
        assert!(!ignored_rec("/elsewhere"));
        // In those, only the open files get through.
        assert!(!ignored_rec("/repo/notes/a.md"));
        assert!(!ignored("/repo/notes/b.md", false));
        assert!(ignored("/repo/notes/other.md", false));
        assert!(ignored("/repo/notes/sub", true));
        assert!(!ignored_rec("/elsewhere/c.md"));
        assert!(ignored("/elsewhere/d.md", false));
        assert!(ignored_rec("/elsewhere/sub/e.md"));
        // An ignored open file in a watched directory gets through, and the
        // root watch is otherwise unchanged.
        assert!(!ignored("/repo/src/debug.log", false));
        assert!(ignored("/repo/src/other.log", false));
        assert!(!ignored("/repo/src/lib.rs", false));
    }

    /// Each path takes the ignore files of the root it's under, whatever order
    /// the roots are in.
    #[test]
    fn ignore_files_of_the_root_a_path_is_under() {
        use std::sync::Arc;

        use filesentry::Filter;
        use ignore::gitignore::{Gitignore, GitignoreBuilder};

        use crate::file_watcher::{IgnoreFiles, WatchFilter};

        let ignore_files = |root: &str, line: &str| {
            let mut builder = GitignoreBuilder::new(root);
            builder.add_line(None, line).unwrap();
            IgnoreFiles {
                root: root.into(),
                ignores: vec![Arc::new(builder.build().unwrap())],
            }
        };
        // LSP roots in order, then the workspace, as `Watcher` builds them
        let filter = WatchFilter {
            filesentry_ignores: Gitignore::empty(),
            ignore_files: vec![
                ignore_files("/a", "a-ignored"),
                ignore_files("/b", "b-ignored"),
                ignore_files("/w", "w-ignored"),
            ],
            global_ignores: Vec::new(),
            hidden: false,
            watch_vcs: true,
            open_files: HashSet::new(),
            open_dirs: HashSet::new(),
        };
        for root in ["/a", "/b", "/w"] {
            let name = &root[1..];
            let ignored = format!("{root}/{name}-ignored");
            assert!(
                filter.ignore_path(Path::new(&ignored), Some(false)),
                "{ignored}"
            );
            assert!(
                filter.ignore_path_rec(Path::new(&ignored), Some(false)),
                "{ignored}"
            );
            let kept = format!("{root}/src/main.rs");
            assert!(
                !filter.ignore_path_rec(Path::new(&kept), Some(false)),
                "{kept}"
            );
        }
    }

    /// A language server's root in a watched root is covered by it, unless it's
    /// in an ignored directory. `Watcher::add_root` adds only the ones not
    /// covered.
    #[test]
    fn covers_nested_roots_unless_ignored() {
        use std::sync::Arc;

        use ignore::gitignore::{Gitignore, GitignoreBuilder};

        use crate::file_watcher::{IgnoreFiles, WatchFilter};

        let mut builder = GitignoreBuilder::new("/repo");
        builder.add_line(None, "vendor/").unwrap();
        let root = |root: &str, ignores| IgnoreFiles {
            root: root.into(),
            ignores,
        };
        let filter = WatchFilter {
            filesentry_ignores: Gitignore::empty(),
            ignore_files: vec![
                root("/repo", vec![Arc::new(builder.build().unwrap())]),
                root("/repo/vendor/dep", Vec::new()),
            ],
            global_ignores: Vec::new(),
            hidden: false,
            watch_vcs: true,
            open_files: HashSet::new(),
            open_dirs: HashSet::new(),
        };
        let covers = |dir: &str| filter.covers(Path::new(dir), Some(true));

        assert!(covers("/repo"));
        assert!(covers("/repo/crates/app"));
        assert!(!covers("/repo/vendor/other"));
        // in the nested root, though the outer root ignores it
        assert!(covers("/repo/vendor/dep"));
        assert!(covers("/repo/vendor/dep/src"));
        assert!(!covers("/elsewhere"));
    }

    /// `hidden` hides hidden files from the watcher, as it does in the file
    /// picker. Off by default, so dotfiles like .eslintrc stay watched.
    #[test]
    fn hidden_ignores_hidden_files() {
        use filesentry::Filter;
        use ignore::gitignore::Gitignore;

        use crate::file_watcher::{Config, WatchFilter};

        assert!(!Config::default().hidden);
        let filter = |hidden| WatchFilter {
            filesentry_ignores: Gitignore::empty(),
            ignore_files: Vec::new(),
            global_ignores: Vec::new(),
            hidden,
            watch_vcs: true,
            open_files: HashSet::new(),
            open_dirs: HashSet::new(),
        };
        let path = Path::new("/repo/.eslintrc");
        assert!(filter(true).ignore_path(path, Some(false)));
        assert!(!filter(false).ignore_path(path, Some(false)));
        // .helix and the VCS files stay watched either way
        assert!(!filter(true).ignore_path(Path::new("/repo/.helix"), Some(true)));
        assert!(!filter(true).ignore_path(Path::new("/repo/.git"), Some(true)));
        assert!(!filter(true).ignore_path(Path::new("/repo/.git/HEAD"), Some(false)));
    }
}
