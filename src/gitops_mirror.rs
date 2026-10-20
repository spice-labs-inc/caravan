//! Bare local mirror of app-gitops, kept under the data directory and read
//! with the git CLI. Replaces the GitHub contents API for everything that
//! only needs file contents from main. Every call blocks on a git process,
//! so async callers wrap them in `tokio::task::spawn_blocking`.

#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const MIRROR_DIR_NAME: &str = "app-gitops.git";

#[derive(Debug, Clone)]
pub struct Mirror {
    path: PathBuf,
    remote: String,
}

/// Where the mirror lives under a data directory.
pub fn mirror_path(data_dir: &Path) -> PathBuf {
    data_dir.join(MIRROR_DIR_NAME)
}

impl Mirror {
    /// Open the mirror at `<data_dir>/app-gitops.git`, cloning it with
    /// `git clone --mirror` when it does not exist yet.
    pub fn open_or_clone(data_dir: &Path, remote: &str) -> Result<Mirror> {
        let path = mirror_path(data_dir);
        if !path.join("HEAD").exists() {
            std::fs::create_dir_all(data_dir)
                .with_context(|| format!("Failed to create {}", data_dir.display()))?;
            if path.exists() {
                std::fs::remove_dir_all(&path).with_context(|| {
                    format!("Failed to remove partial mirror {}", path.display())
                })?;
            }
            tracing::info!(remote = %remote, path = %path.display(), "Cloning app-gitops mirror");
            run_git(
                None,
                &[
                    "clone",
                    "--mirror",
                    remote,
                    path.to_str().unwrap_or_default(),
                ],
            )?;
        }
        Ok(Mirror {
            path,
            remote: remote.to_string(),
        })
    }

    /// Open an existing mirror without cloning. None when there is none.
    pub fn open_existing(data_dir: &Path, remote: &str) -> Option<Mirror> {
        let path = mirror_path(data_dir);
        path.join("HEAD").exists().then(|| Mirror {
            path,
            remote: remote.to_string(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn remote(&self) -> &str {
        &self.remote
    }

    pub fn fetch(&self) -> Result<()> {
        self.git(&["fetch", "--prune", "origin"]).map(|_| ())
    }

    pub fn head_sha(&self, branch: &str) -> Result<String> {
        let out = self.git(&["rev-parse", &format!("{branch}^{{commit}}")])?;
        Ok(out.trim().to_string())
    }

    /// Number of commits reachable from `to` but not from `from`.
    pub fn commits_between(&self, from: &str, to: &str) -> Result<u64> {
        let out = self.git(&["rev-list", "--count", &format!("{from}..{to}")])?;
        out.trim()
            .parse::<u64>()
            .with_context(|| format!("Unexpected rev-list output: {}", out.trim()))
    }

    /// File contents at `rev:path`, None when the path is not in that tree.
    pub fn read_file(&self, rev: &str, path: &str) -> Result<Option<String>> {
        let spec = format!("{rev}:{path}");
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.path)
            .args(["show", &spec])
            .output()
            .context("Failed to run git show")?;
        if output.status.success() {
            return Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("does not exist") || stderr.contains("exists on disk, but not in") {
            return Ok(None);
        }
        bail!("git show {} failed: {}", spec, stderr.trim());
    }

    /// Names of the directories directly under `rev:path`.
    pub fn list_dirs(&self, rev: &str, path: &str) -> Result<Vec<String>> {
        let spec = format!("{rev}:{path}");
        let out = self.git(&["ls-tree", &spec])?;
        let mut dirs: Vec<String> = out
            .lines()
            .filter_map(|line| {
                let (meta, name) = line.split_once('\t')?;
                let mut fields = meta.split_whitespace();
                let _mode = fields.next()?;
                let kind = fields.next()?;
                (kind == "tree").then(|| name.to_string())
            })
            .collect();
        dirs.sort();
        Ok(dirs)
    }

    /// Names of the files directly under `rev:path`.
    pub fn list_files(&self, rev: &str, path: &str) -> Result<Vec<String>> {
        let spec = format!("{rev}:{path}");
        let out = self.git(&["ls-tree", &spec])?;
        let mut files: Vec<String> = out
            .lines()
            .filter_map(|line| {
                let (meta, name) = line.split_once('\t')?;
                let kind = meta.split_whitespace().nth(1)?;
                (kind == "blob").then(|| name.to_string())
            })
            .collect();
        files.sort();
        Ok(files)
    }

    /// Working clone of `branch` at `dest`, with origin pointing at GitHub so
    /// a push from there lands upstream rather than in the mirror.
    pub fn clone_to(&self, dest: &Path, branch: &str) -> Result<()> {
        if dest.exists() {
            bail!("{} already exists", dest.display());
        }
        run_git(
            None,
            &[
                "clone",
                "--branch",
                branch,
                self.path.to_str().unwrap_or_default(),
                dest.to_str().unwrap_or_default(),
            ],
        )?;
        run_git(Some(dest), &["remote", "set-url", "origin", &self.remote])?;
        Ok(())
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        run_git(Some(&self.path), args)
    }
}

fn run_git(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    if let Some(dir) = dir {
        cmd.arg("-C").arg(dir);
    }
    let output = cmd
        .args(args)
        .output()
        .with_context(|| format!("Failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::var_os("CARAVAN_TEST_SCRATCH")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("caravan-test-{}-{}", name, std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sh(dir: &Path, args: &[&str]) {
        let st = Command::new(args[0])
            .args(&args[1..])
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(st.success(), "{:?} failed", args);
    }

    #[test]
    fn mirror_reads_a_local_repo() {
        let root = scratch("mirror");
        let src = root.join("src");
        std::fs::create_dir_all(src.join("apps/base/notes-api")).unwrap();
        std::fs::create_dir_all(src.join("apps/base/operator")).unwrap();
        std::fs::write(src.join("apps/base/notes-api/patch.yml"), "a: 1\n").unwrap();
        std::fs::write(src.join("apps/base/operator/install.yml"), "b: 2\n").unwrap();
        std::fs::write(src.join("apps/base/file.yml"), "c: 3\n").unwrap();
        sh(&src, &["git", "init", "-q", "-b", "main"]);
        sh(
            &src,
            &[
                "git",
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "add",
                ".",
            ],
        );
        sh(
            &src,
            &[
                "git",
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "one",
            ],
        );

        let data = root.join("data");
        let mirror = Mirror::open_or_clone(&data, src.to_str().unwrap()).unwrap();
        assert!(Mirror::open_existing(&data, "x").is_some());
        let head1 = mirror.head_sha("main").unwrap();
        assert_eq!(head1.len(), 40);
        assert_eq!(
            mirror.list_dirs("main", "apps/base").unwrap(),
            vec!["notes-api".to_string(), "operator".to_string()]
        );
        assert_eq!(
            mirror
                .read_file("main", "apps/base/notes-api/patch.yml")
                .unwrap(),
            Some("a: 1\n".to_string())
        );
        assert_eq!(
            mirror.read_file("main", "apps/base/nope.yml").unwrap(),
            None
        );

        std::fs::write(src.join("apps/base/notes-api/patch.yml"), "a: 2\n").unwrap();
        sh(
            &src,
            &[
                "git",
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-am",
                "two",
            ],
        );
        mirror.fetch().unwrap();
        let head2 = mirror.head_sha("main").unwrap();
        assert_ne!(head1, head2);
        assert_eq!(mirror.commits_between(&head1, "main").unwrap(), 1);

        let dest = root.join("work");
        mirror.clone_to(&dest, "main").unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("apps/base/notes-api/patch.yml")).unwrap(),
            "a: 2\n"
        );
        let origin = Command::new("git")
            .args(["-C", dest.to_str().unwrap(), "remote", "get-url", "origin"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&origin.stdout).trim(),
            src.to_str().unwrap()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Live check against the real app-gitops mirror over ssh. Run manually:
    /// cargo test mirror_live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn mirror_live() {
        let root = scratch("mirror-live");
        let mirror =
            Mirror::open_or_clone(&root, "git@github.com:notes-co/app-gitops.git").unwrap();
        mirror.fetch().unwrap();
        println!("main = {}", mirror.head_sha("main").unwrap());
        let dirs = mirror.list_dirs("main", "apps/base").unwrap();
        println!("apps/base dirs = {:?}", dirs);
        let base = mirror
            .read_file("main", "apps/base/operator/install.yml")
            .unwrap()
            .expect("base operator install.yml");
        let staging = mirror
            .read_file("main", "apps/staging/operator/config-patch.yml")
            .unwrap()
            .expect("staging operator config-patch.yml");
        let op = crate::gitops::overlay::OperatorConfig {
            primary_component: Some("engine".into()),
            primary_image_key: Some("target_image".into()),
            ..crate::gitops::overlay::OperatorConfig::new("operator")
        };
        let pins = op.parse_pins(&base, Some(&staging));
        println!("staging operator pins = {:#?}", pins);
        assert!(pins.components.contains_key("engine"));
        assert!(pins.components.contains_key("sqld"));
        std::fs::remove_dir_all(&root).ok();
    }
}
