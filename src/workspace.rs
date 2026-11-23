//! The specs of the workspace, linted as they are on disk for the
//! diagnostics of those not open, and linted again when the files their
//! findings depend on change.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use lsp_types::{FileSystemWatcher, GlobPattern};

use crate::lint;

/// How deep the specs are looked for under the roots: `<package>/` for
/// dist-git, `<repository>/<package>/` for xcraft, without walking a whole
/// home directory.
const DEPTH: usize = 3;

pub struct Workspace {
    specs: BTreeSet<PathBuf>,
    /// The configuration files of the linter, resolved, as the editor
    /// reports the changes of the files links point to.
    configs: BTreeSet<PathBuf>,
}

impl Workspace {
    /// The specs under `roots`, but in hidden directories and `target`.
    pub fn scan(roots: &[PathBuf], config: &lint::Config) -> Self {
        let mut specs = BTreeSet::new();
        for root in roots {
            find_specs(root, DEPTH, &mut specs);
        }
        let configs = config
            .files
            .iter()
            .flat_map(|path| match std::fs::read_dir(path) {
                Ok(entries) => entries.filter_map(|e| Some(e.ok()?.path())).collect(),
                Err(_) => vec![path.clone()],
            })
            .filter(|path| path.extension().is_some_and(|e| e == "toml"))
            .filter_map(|path| path.canonicalize().ok())
            .collect();
        Self { specs, configs }
    }

    pub fn specs(&self) -> Vec<PathBuf> {
        self.specs.iter().cloned().collect()
    }

    /// Add the spec of an open document, linted again with the others.
    pub fn add(&mut self, spec: PathBuf) {
        if spec.extension().is_some_and(|e| e == "spec") {
            self.specs.insert(spec);
        }
    }

    /// The files whose changes the editor reports: the specs, the
    /// rpmlintrc files, and the configuration.
    pub fn watchers(&self) -> Vec<FileSystemWatcher> {
        let globs = ["**/*.spec".to_string(), "**/*rpmlintrc".to_string()];
        let configs = self.configs.iter().map(|p| p.to_string_lossy().into_owned());
        globs
            .into_iter()
            .chain(configs)
            .map(|glob| FileSystemWatcher { glob_pattern: GlobPattern::String(glob), kind: None })
            .collect()
    }

    /// The specs to lint again when `path` changed: itself for a spec, added
    /// to or removed from the workspace, those an rpmlintrc applies to, and
    /// all of them for the configuration.
    pub fn affected(&mut self, path: &Path, deleted: bool, config: &lint::Config) -> Vec<PathBuf> {
        let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
        if name.ends_with(".spec") {
            if deleted {
                self.specs.remove(path);
            } else {
                self.specs.insert(path.to_path_buf());
            }
            return vec![path.to_path_buf()];
        }
        if name.ends_with("rpmlintrc") {
            let package = (path.parent() == config.rpmlintrc_dir.as_deref())
                .then(|| name.strip_suffix(".rpmlintrc"))
                .flatten();
            return self
                .specs
                .iter()
                .filter(|spec| match package {
                    Some(package) => {
                        spec.parent().and_then(Path::file_name)
                            == Some(std::ffi::OsStr::new(package))
                    }
                    None => spec.parent() == path.parent(),
                })
                .cloned()
                .collect();
        }
        let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if self.configs.iter().any(|c| resolved.starts_with(c)) {
            return self.specs();
        }
        Vec::new()
    }
}

fn find_specs(dir: &Path, depth: usize, specs: &mut BTreeSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let (path, name) = (entry.path(), entry.file_name());
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            if depth > 0 && name != "target" {
                find_specs(&path, depth - 1, specs);
            }
        } else if name.ends_with(".spec") {
            specs.insert(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_are_found_and_linted_again_with_what_they_depend_on() {
        let dir = std::env::temp_dir().join(format!("rpmls-workspace-{}", std::process::id()));
        let root = dir.join("project");
        for d in ["base/foo", "base/bar", ".xcraft/build/foo", "lint", "a/b/c/d"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        for f in ["base/foo/foo.spec", "base/bar/bar.spec", ".xcraft/build/foo/foo.spec"] {
            std::fs::write(root.join(f), "").unwrap();
        }
        std::fs::write(root.join("a/b/c/d/deep.spec"), "").unwrap();
        // The configuration through a link, as xcraft gives it.
        std::fs::write(root.join("lint/rpmlint.toml"), "").unwrap();
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::os::unix::fs::symlink(root.join("lint/rpmlint.toml"), dir.join("config/rpmlint.toml"))
            .unwrap();
        let config = lint::Config {
            files: vec![dir.join("config")],
            rpmlintrc_dir: Some(root.join("lint")),
            ..Default::default()
        };
        let mut ws = Workspace::scan(std::slice::from_ref(&root), &config);
        let (foo, bar) = (root.join("base/foo/foo.spec"), root.join("base/bar/bar.spec"));
        // Not in hidden directories, nor too deep.
        assert_eq!(ws.specs(), [bar.clone(), foo.clone()]);
        assert!(ws.watchers().iter().any(|w| matches!(&w.glob_pattern,
            GlobPattern::String(g) if g.ends_with("project/lint/rpmlint.toml"))));
        // The package's rpmlintrc, out of its directory, or one beside it.
        assert_eq!(ws.affected(&root.join("lint/foo.rpmlintrc"), false, &config), [foo]);
        assert_eq!(ws.affected(&root.join("base/bar/bar.rpmlintrc"), false, &config), [bar]);
        assert_eq!(ws.affected(&root.join("lint/rpmlint.toml"), false, &config).len(), 2);
        // A spec, added and removed.
        let baz = root.join("base/baz/baz.spec");
        assert_eq!(ws.affected(&baz, false, &config), std::slice::from_ref(&baz));
        assert_eq!(ws.specs().len(), 3);
        ws.affected(&baz, true, &config);
        assert_eq!(ws.specs().len(), 2);
        assert!(ws.affected(&root.join("README.md"), false, &config).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
