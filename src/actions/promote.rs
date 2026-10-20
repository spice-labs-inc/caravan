//! Promote one environment's pins to another. The change set comes from the
//! configured gitops layout (pure, testable), is applied by in-place string
//! replacement in a scratch clone of the gitops repository, then committed
//! and pushed to the configured branch.

use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub use crate::gitops::{ChangeSet, ServiceChange, ValueEdit};
use crate::gitops_mirror::Mirror;

#[derive(Debug, Clone)]
pub struct PromoteOutcome {
    pub sha: String,
    pub changed_files: Vec<String>,
}

#[derive(Debug)]
pub enum PromoteError {
    /// Remote branch moved since the clone; re-clone and recompute
    PushRejected(String),
}

impl std::fmt::Display for PromoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromoteError::PushRejected(msg) => write!(f, "push rejected: {msg}"),
        }
    }
}

impl std::error::Error for PromoteError {}

/// Commit message for the selected part of a change set.
pub fn commit_message(cs: &ChangeSet, selected: &HashSet<String>) -> String {
    let mut names: Vec<String> = Vec::new();
    for svc in cs.services.iter().filter(|s| selected.contains(&s.name)) {
        if svc.components.is_empty() {
            names.push(svc.name.clone());
        } else {
            names.extend(svc.components.iter().cloned());
        }
    }
    format!("promote {} to {}: {}", cs.from, cs.to, names.join(", "))
}

/// Rewrite the selected services' target files under `dir`, replacing each
/// old value on its own key line so the rest of the file is untouched.
/// Returns the repo-relative paths that changed.
pub fn apply_change_set(
    dir: &Path,
    cs: &ChangeSet,
    selected: &HashSet<String>,
) -> Result<Vec<String>> {
    let mut changed = Vec::new();
    for svc in cs.services.iter().filter(|s| selected.contains(&s.name)) {
        let path = dir.join(&svc.file);
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        ensure_no_conflict_markers(&path, &content)?;
        let mut updated = content.clone();
        for edit in &svc.edits {
            updated = replace_on_key_line(&updated, edit)
                .with_context(|| format!("{}: {}", svc.file, edit.path))?;
        }
        if updated != content {
            std::fs::write(&path, updated)
                .with_context(|| format!("Failed to write {}", path.display()))?;
            changed.push(svc.file.clone());
        }
    }
    Ok(changed)
}

/// Replace `old` with `new` only on lines whose yaml key is the edit's last
/// path segment (or the whole operator data key).
fn replace_on_key_line(content: &str, edit: &ValueEdit) -> Result<String> {
    let key = edit.path.rsplit('.').next().unwrap_or(&edit.path);
    let full_key = edit.path.as_str();
    let mut hit = false;
    let mut already = false;
    let mut out = String::with_capacity(content.len());
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let line_key = trimmed.split(':').next().unwrap_or("").trim();
        let key_matches = line_key == key || line_key == full_key;
        if key_matches && line.contains(&edit.old) {
            out.push_str(&line.replacen(&edit.old, &edit.new, 1));
            hit = true;
        } else {
            if key_matches && line.contains(&edit.new) {
                already = true;
            }
            out.push_str(line);
        }
    }
    if !hit && !already {
        bail!("value {} not found on a {} line", edit.old, key);
    }
    Ok(out)
}

/// Refuse to edit files carrying unresolved git conflict markers: string
/// replacement on top of a conflicted file produces YAML that breaks the
/// whole kustomization for the environment.
fn ensure_no_conflict_markers(path: &Path, content: &str) -> Result<()> {
    if content
        .lines()
        .any(|l| l.starts_with("<<<<<<<") || l.starts_with(">>>>>>>"))
    {
        bail!(
            "{} has unresolved git conflict markers; resolve them before promoting",
            path.display()
        );
    }
    Ok(())
}

/// Stage everything, commit, push to origin `branch`. Returns the new sha.
/// A non-fast-forward rejection surfaces as `PromoteError::PushRejected`.
pub fn commit_and_push(dir: &Path, branch: &str, message: &str) -> Result<String> {
    git(dir, &["add", "-A"])?;
    // A scratch clone on a machine without a git identity (CI) still commits.
    let has_identity = git(dir, &["config", "user.email"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if has_identity {
        git(dir, &["commit", "-q", "-m", message])?;
    } else {
        let name = format!("user.name={}", crate::branding::DISPLAY_NAME);
        let email = format!("user.email={}@localhost", crate::branding::APP_NAME);
        git(
            dir,
            &["-c", &name, "-c", &email, "commit", "-q", "-m", message],
        )?;
    }
    let sha = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    let refspec = format!("HEAD:{branch}");
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["push", "origin", &refspec])
        .output()
        .context("Failed to run git push")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.contains("rejected")
            || stderr.contains("non-fast-forward")
            || stderr.contains("fetch first")
        {
            return Err(PromoteError::PushRejected(stderr).into());
        }
        bail!("git push failed: {stderr}");
    }
    Ok(sha)
}

/// Fresh clone of `branch` next to the mirror, apply, commit and push, then
/// remove the clone whatever happened.
pub fn run_promote(
    mirror: &Mirror,
    branch: &str,
    cs: &ChangeSet,
    selected: &HashSet<String>,
    message: &str,
    progress: &dyn Fn(&str),
) -> Result<PromoteOutcome> {
    let scratch = scratch_dir(mirror);
    let result = (|| {
        progress("cloning");
        mirror.clone_to(&scratch, branch)?;
        progress("applying");
        let changed_files = apply_change_set(&scratch, cs, selected)?;
        if changed_files.is_empty() {
            bail!("nothing to promote");
        }
        progress("pushing");
        let sha = commit_and_push(&scratch, branch, message)?;
        Ok(PromoteOutcome { sha, changed_files })
    })();
    if let Err(e) = std::fs::remove_dir_all(&scratch) {
        if scratch.exists() {
            tracing::warn!(path = %scratch.display(), err = %e, "Failed to remove promote scratch clone");
        }
    }
    result
}

fn scratch_dir(mirror: &Mirror) -> PathBuf {
    let root = mirror
        .path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(crate::config::data_dir);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    root.join(format!("promote-{ts}-{}", std::process::id()))
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
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
    use crate::gitops::overlay::{OperatorConfig, OverlayConfig, OverlayLayout};
    use crate::gitops::{GitopsLayout, GitopsTree, MapTree, MirrorTree};
    use std::collections::HashMap;
    use std::process::Command;

    /// The layout the fixtures use.
    fn layout() -> OverlayLayout {
        OverlayLayout::new(OverlayConfig {
            operator: Some(OperatorConfig {
                primary_component: Some("engine".into()),
                primary_image_key: Some("target_image".into()),
                ..OperatorConfig::new("operator")
            }),
            ..OverlayConfig::default()
        })
    }

    fn change_set(
        tree: &dyn GitopsTree,
        sha: &str,
        services: &[String],
        ahead: &HashMap<String, i64>,
    ) -> ChangeSet {
        layout()
            .promotion(tree, sha, "staging", "dev", services, ahead)
            .unwrap()
    }

    fn map_tree(files: HashMap<&str, &str>) -> MapTree {
        MapTree(
            files
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    const WORKER_STAGING: &str = r#"apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: notes-worker
spec:
  chart:
    spec:
      version: 0.1.0-27ef94dc1fe4148af50c1ecd2b95f0a2e27bcc54
  values:
    image:
      registry: ghcr.io/notes-co
    worker:
      image:
        tag: tree-3438098b2d850f296438bfe9d15b485e47c4077d
        digest: sha256:5af0a017e7dff8f41782113a42698a26956b1871a357b216e543e298995fb468
    ui:
      image:
        tag: tree-c63b2527040be51952f47711eeb1bd5a58ef6539
        digest: sha256:bec940e59cc9d0c56bd23b34eef95d217e66aa501c367eedf75a527f003891fb
    sampleData:
      image:
        tag: tree-f59eb665d87374d9614be653d11028e4186d9336
        digest: sha256:064b6d0e4ef109895cf1e676f67894d13b1829fc85df4cddab9ff08e51f8cb2c
    pqcOrchestrator:
      image:
        tag: tree-a1030c56842436bc88b9db47fe6972e32d1e8ac5
        digest: sha256:b9acf54ae2e643ca71beff2966eb80ee81cc898e2c5bf81bec4e712b7e4d015f
"#;

    const WORKER_DEV: &str = r#"apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: notes-worker
spec:
  chart:
    spec:
      version: 0.1.0-1111111111111111111111111111111111111111
  values:
    image:
      registry: ghcr.io/notes-co
    replicaCount: 2   # dev only
    worker:
      image:
        tag: tree-3438098b2d850f296438bfe9d15b485e47c4077d
        digest: sha256:5af0a017e7dff8f41782113a42698a26956b1871a357b216e543e298995fb468
    ui:
      image:
        tag: tree-2222222222222222222222222222222222222222
        digest: sha256:2222222222222222222222222222222222222222222222222222222222222222
    sampleData:
      image:
        tag: tree-f59eb665d87374d9614be653d11028e4186d9336
        digest: sha256:064b6d0e4ef109895cf1e676f67894d13b1829fc85df4cddab9ff08e51f8cb2c
    pqcOrchestrator:
      image:
        tag: tree-3333333333333333333333333333333333333333
        digest: sha256:b9acf54ae2e643ca71beff2966eb80ee81cc898e2c5bf81bec4e712b7e4d015f
"#;

    const NOTES_WEB_STAGING: &str = r#"spec:
  chart:
    spec:
      version: 0.1.0-61ace0a101a475d9bf2b7c63da376a5ff88895d1
  values:
    api:
      image:
        tag: pr-11@sha256:0d2585f6f2fc03aff453c993fc4fad13d81363ca41f350d180c3dbd8b0796540
    ingest:
      image:
        tag: pr-11@sha256:0f51edb4be6a7bd88a5becabe72e86d03f4a633a05745cd52d4326f9c5d3651a
"#;

    const NOTES_WEB_DEV: &str = r#"spec:
  chart:
    spec:
      version: 0.1.0-61ace0a101a475d9bf2b7c63da376a5ff88895d1
  values:
    api:
      image:
        tag: pr-9@sha256:9999999999999999999999999999999999999999999999999999999999999999
    ingest:
      image:
        tag: pr-11@sha256:0f51edb4be6a7bd88a5becabe72e86d03f4a633a05745cd52d4326f9c5d3651a
"#;

    const NOTES_API_STAGING: &str = r#"apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: notes-api
spec:
  chart:
    spec:
      version: 0.1.0-38f2c7ba3b853148b11662c429dce4a53b928482
  values:
    image:
      tag: sha-38f2c7ba3b853148b11662c429dce4a53b928482
    publicApiUrl: "https://api.staging.notes-co.example"
    pause:
      enabled: true
      timeoutSeconds: 7200
"#;

    const NOTES_API_DEV: &str = r#"apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: notes-api
spec:
  chart:
    spec:
      version: 0.1.0-38f2c7ba3b853148b11662c429dce4a53b928482
  values:
    image:
      tag: sha-38f2c7ba3b853148b11662c429dce4a53b928482
    publicApiUrl: "https://api.dev.notes-co.example"
    pause:
      enabled: true
      timeoutSeconds: 600
"#;

    const OPERATOR_STAGING: &str = r#"apiVersion: v1
kind: ConfigMap
metadata:
  name: config
data:
  chart_name: engine
  chart_version: 1.0.0-bedcc26732af16ab6a55811a6b7f4c7b8fb842a5
  target_image: ghcr.io/notes-co/engine:sha-e3f01e86a467c48fed87c4de092ec00343b28c1f@sha256:44009d72cc98b13eec906dde0f0498a24901d951a6cc6548b2fc65d96e65d77b
  values.crypto_image: ghcr.io/notes-co/crypto:sha-e3f01e86a467c48fed87c4de092ec00343b28c1f@sha256:42060262e5048e5ea09499dde0b20d99595c2254d3423c9247ac6a63d7367b15
  fuse_sidecar_image: ghcr.io/notes-co/engine-fuse-sidecar:sha-f058488f480162fc3a27768cef67905c43eea673
  values.broker_image: ghcr.io/notes-co/broker:sha-1e758420189b97dd2fed99f97c35e7e1c6510f45@sha256:764cc7326a9fbe2812f5ccd689877949c4335821bfe4fc577798b7fe68fb40ab
  values.engine_image: ghcr.io/notes-co/engine:sha-e3f01e86a467c48fed87c4de092ec00343b28c1f@sha256:44009d72cc98b13eec906dde0f0498a24901d951a6cc6548b2fc65d96e65d77b
  values.broker.mintJobs: "true"
"#;

    const OPERATOR_DEV: &str = r#"apiVersion: v1
kind: ConfigMap
metadata:
  name: config
data:
  chart_name: engine
  chart_version: 1.0.0-bedcc26732af16ab6a55811a6b7f4c7b8fb842a5
  target_image: ghcr.io/notes-co/engine:pr-612@sha256:44009d72cc98b13eec906dde0f0498a24901d951a6cc6548b2fc65d96e65d77b
  values.crypto_image: ghcr.io/notes-co/crypto:pr-612@sha256:42060262e5048e5ea09499dde0b20d99595c2254d3423c9247ac6a63d7367b15
  values.broker_image: ghcr.io/notes-co/broker:pr-620@sha256:7814242c2522d9beb70c2cbe12b0b15e8308e260a5fe380fba35a10bfbeab555
  values.engine_image: ghcr.io/notes-co/engine:pr-612@sha256:44009d72cc98b13eec906dde0f0498a24901d951a6cc6548b2fc65d96e65d77b
  values.broker.mintJobs: "true"
  values.sqld_image: "ghcr.io/notes-co/libsql-server:a64d504@sha256:f07e8119b33a266da781329f427b020649104de660c2a181dab12bed28d04412"
  values.truffle_url: "https://truffle.notes-co.example"
"#;

    fn fixture_files() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            (
                "apps/staging/notes-worker/notes-worker-patch.yml",
                WORKER_STAGING,
            ),
            ("apps/dev/notes-worker/notes-worker-patch.yml", WORKER_DEV),
            (
                "apps/staging/notes-web/notes-web-patch.yml",
                NOTES_WEB_STAGING,
            ),
            ("apps/dev/notes-web/notes-web-patch.yml", NOTES_WEB_DEV),
            (
                "apps/staging/notes-api/notes-api-patch.yml",
                NOTES_API_STAGING,
            ),
            ("apps/dev/notes-api/notes-api-patch.yml", NOTES_API_DEV),
            (
                "apps/staging/staging-only/staging-only-patch.yml",
                NOTES_API_STAGING,
            ),
            ("apps/staging/operator/config-patch.yml", OPERATOR_STAGING),
            ("apps/dev/operator/config-patch.yml", OPERATOR_DEV),
        ])
    }

    fn services() -> Vec<String> {
        [
            "notes-worker",
            "notes-web",
            "notes-api",
            "operator",
            "staging-only",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn fixture_change_set(ahead: HashMap<String, i64>) -> ChangeSet {
        change_set(
            &map_tree(fixture_files()),
            "abcdef0123456789",
            &services(),
            &ahead,
        )
    }

    fn edits_of<'a>(cs: &'a ChangeSet, name: &str) -> &'a ServiceChange {
        cs.services.iter().find(|s| s.name == name).unwrap()
    }

    #[test]
    fn yaml_patches_are_promoted() {
        let files = HashMap::from([
            (
                "apps/staging/notes-docs/notes-docs-patch.yaml",
                NOTES_WEB_STAGING,
            ),
            ("apps/dev/notes-docs/notes-docs-patch.yaml", NOTES_WEB_DEV),
        ]);
        let cs = change_set(
            &map_tree(files),
            "abc",
            &["notes-docs".to_string()],
            &HashMap::new(),
        );
        let docs = edits_of(&cs, "notes-docs");
        assert_eq!(docs.file, "apps/dev/notes-docs/notes-docs-patch.yaml");
        assert!(!docs.edits.is_empty());
    }

    #[test]
    fn change_set_matches_every_nested_pin() {
        let cs = fixture_change_set(HashMap::from([("broker".to_string(), 3)]));
        assert_eq!(cs.base_sha, "abcdef0123456789");
        let names: Vec<&str> = cs.services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["notes-worker", "notes-web", "operator"]);
        assert_eq!(cs.unchanged, vec!["notes-api".to_string()]);

        let worker = edits_of(&cs, "notes-worker");
        assert_eq!(worker.file, "apps/dev/notes-worker/notes-worker-patch.yml");
        let paths: Vec<&str> = worker.edits.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "chart.spec.version",
                "values.pqcOrchestrator.image.tag",
                "values.ui.image.digest",
                "values.ui.image.tag",
            ]
        );
        assert!(worker.warning.is_none());

        let web = edits_of(&cs, "notes-web");
        assert_eq!(web.edits.len(), 1);
        assert_eq!(web.edits[0].path, "values.api.image.tag");
        assert!(web.edits[0].new.starts_with("pr-11@sha256:"));

        let operator = edits_of(&cs, "operator");
        let paths: Vec<&str> = operator.edits.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "target_image",
                "values.broker_image",
                "values.crypto_image",
                "values.engine_image",
            ]
        );
        assert_eq!(
            cs.operator_target_only,
            vec![
                "values.sqld_image".to_string(),
                "values.truffle_url".to_string()
            ]
        );
        assert_eq!(operator.components, vec!["engine", "broker", "crypto"]);
        assert_eq!(
            operator.warning.as_deref(),
            Some("3 dev is ahead here (broker) · promoting moves it back to the staging build")
        );
        assert_eq!(
            commit_message(
                &cs,
                &["notes-web".to_string(), "operator".to_string()].into()
            ),
            "promote staging to dev: notes-web, engine, broker, crypto"
        );
    }

    #[test]
    fn apply_replaces_values_in_place() {
        let cs = fixture_change_set(HashMap::new());
        let root = scratch("apply");
        for (path, content) in fixture_files() {
            let p = root.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
        }
        let selected: HashSet<String> = cs.services.iter().map(|s| s.name.clone()).collect();
        let mut changed = apply_change_set(&root, &cs, &selected).unwrap();
        changed.sort();
        assert_eq!(
            changed,
            vec![
                "apps/dev/notes-web/notes-web-patch.yml",
                "apps/dev/notes-worker/notes-worker-patch.yml",
                "apps/dev/operator/config-patch.yml",
            ]
        );

        let worker =
            std::fs::read_to_string(root.join("apps/dev/notes-worker/notes-worker-patch.yml"))
                .unwrap();
        assert!(worker.contains("    replicaCount: 2   # dev only\n"));
        assert!(worker.contains("version: 0.1.0-27ef94dc1fe4148af50c1ecd2b95f0a2e27bcc54"));
        assert!(worker.contains("tag: tree-c63b2527040be51952f47711eeb1bd5a58ef6539"));
        assert!(worker.contains("tag: tree-a1030c56842436bc88b9db47fe6972e32d1e8ac5"));
        assert!(!worker.contains("2222"));
        assert_eq!(worker.lines().count(), WORKER_DEV.lines().count());

        let operator =
            std::fs::read_to_string(root.join("apps/dev/operator/config-patch.yml")).unwrap();
        assert!(operator.contains("  values.sqld_image: \"ghcr.io/notes-co/libsql-server"));
        assert!(operator.contains("  values.truffle_url: \"https://truffle.notes-co.example\""));
        assert!(!operator.contains("fuse_sidecar_image"));
        assert!(!operator.contains("pr-612"));
        assert!(!operator.contains("pr-620"));
        assert_eq!(
            operator
                .matches("sha-e3f01e86a467c48fed87c4de092ec00343b28c1f")
                .count(),
            3
        );

        // Second pass finds nothing to change
        let cs2 = {
            let mut files: HashMap<String, String> = HashMap::new();
            for (p, _) in fixture_files() {
                let f = root.join(p);
                if f.is_file() {
                    files.insert(p.to_string(), std::fs::read_to_string(f).unwrap());
                }
            }
            change_set(&MapTree(files), "x", &services(), &HashMap::new())
        };
        assert!(cs2.services.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn apply_refuses_conflict_markers() {
        let cs = fixture_change_set(HashMap::new());
        let root = scratch("conflict");
        for (path, content) in fixture_files() {
            let p = root.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
        }
        let web = root.join("apps/dev/notes-web/notes-web-patch.yml");
        let conflicted = format!(
            "{}<<<<<<< HEAD\n    x: 1\n=======\n    x: 2\n>>>>>>> other\n",
            NOTES_WEB_DEV
        );
        std::fs::write(&web, conflicted).unwrap();
        let err = apply_change_set(&root, &cs, &["notes-web".to_string()].into()).unwrap_err();
        assert!(format!("{err}").contains("conflict markers"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::var_os("CARAVAN_TEST_SCRATCH")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "caravan-test-promote-{}-{}",
                name,
                std::process::id()
            ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = Command::new(args[0])
            .args(&args[1..])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn git_commit(dir: &Path, msg: &str) {
        sh(dir, &["git", "add", "-A"]);
        sh(
            dir,
            &[
                "git",
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                msg,
            ],
        );
    }

    /// Bare "GitHub" repo, a seed clone with the fixtures, a mirror of the
    /// bare repo, then a full promote through the mirror.
    #[test]
    fn round_trip_pushes_to_local_bare_repo() {
        let root = scratch("roundtrip");
        let bare = root.join("app-gitops-bare.git");
        std::fs::create_dir_all(&bare).unwrap();
        sh(&bare, &["git", "init", "-q", "--bare", "-b", "main"]);

        let seed = root.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        sh(&seed, &["git", "init", "-q", "-b", "main"]);
        for (path, content) in fixture_files() {
            let p = seed.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
        }
        git_commit(&seed, "seed");
        sh(
            &seed,
            &["git", "remote", "add", "origin", bare.to_str().unwrap()],
        );
        sh(&seed, &["git", "push", "-q", "origin", "main"]);
        let seed_sha = sh(&seed, &["git", "rev-parse", "HEAD"]).trim().to_string();

        let data = root.join("data");
        let mirror = Mirror::open_or_clone(&data, bare.to_str().unwrap()).unwrap();
        let head = mirror.head_sha("main").unwrap();
        assert_eq!(head, seed_sha);
        let services = mirror.list_dirs("main", "apps/staging").unwrap();
        let tree = MirrorTree {
            mirror: &mirror,
            rev: "main",
        };
        let cs = change_set(&tree, &head, &services, &HashMap::new());
        assert_eq!(cs.services.len(), 3);

        let selected: HashSet<String> = ["notes-worker".to_string(), "operator".to_string()].into();
        let message = commit_message(&cs, &selected);
        let outcome = run_promote(&mirror, "main", &cs, &selected, &message, &|_| {}).unwrap();
        assert_eq!(outcome.changed_files.len(), 2);

        let bare_head = sh(&bare, &["git", "rev-parse", "main"]).trim().to_string();
        assert_eq!(bare_head, outcome.sha);
        assert_ne!(bare_head, seed_sha);
        let subject = sh(&bare, &["git", "log", "-1", "--format=%s", "main"]);
        assert_eq!(
            subject.trim(),
            "promote staging to dev: notes-worker, engine, broker, crypto"
        );
        let pushed = sh(
            &bare,
            &["git", "show", "main:apps/dev/operator/config-patch.yml"],
        );
        assert!(pushed.contains("values.truffle_url"));
        assert!(!pushed.contains("pr-620"));
        let untouched = sh(
            &bare,
            &["git", "show", "main:apps/dev/notes-web/notes-web-patch.yml"],
        );
        assert_eq!(untouched, NOTES_WEB_DEV);

        let leftovers: Vec<_> = std::fs::read_dir(&data)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("promote-"))
            .collect();
        assert!(leftovers.is_empty(), "scratch clone left behind");

        // A stale clone (the mirror was not fetched) gets a rejected push
        sh(&seed, &["git", "pull", "-q", "--ff-only", "origin", "main"]);
        sh(
            &seed,
            &[
                "git",
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "moved",
            ],
        );
        sh(&seed, &["git", "push", "-q", "origin", "main"]);
        let cs = change_set(&tree, &head, &services, &HashMap::new());
        let selected: HashSet<String> = ["notes-web".to_string()].into();
        let err = run_promote(&mirror, "main", &cs, &selected, "x", &|_| {}).unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<PromoteError>(),
                Some(PromoteError::PushRejected(_))
            ),
            "{err}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Live read of the real mirror over ssh, no apply, no push. Run once:
    /// cargo test promote_change_set_live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn promote_change_set_live() {
        let root = scratch("live");
        let mirror =
            Mirror::open_or_clone(&root, "git@github.com:notes-co/app-gitops.git").unwrap();
        mirror.fetch().unwrap();
        let head = mirror.head_sha("main").unwrap();
        let services = mirror.list_dirs("main", "apps/staging").unwrap();
        let tree = MirrorTree {
            mirror: &mirror,
            rev: "main",
        };
        let cs = change_set(&tree, &head, &services, &HashMap::new());
        println!("main @ {head}");
        println!(
            "{} services change, {} already match: {:?}",
            cs.services.len(),
            cs.unchanged.len(),
            cs.unchanged
        );
        println!("operator dev-only keys: {:?}", cs.operator_target_only);
        for svc in &cs.services {
            println!("{} ({})", svc.name, svc.file);
            for e in &svc.edits {
                println!("  {}: {} -> {}", e.path, e.old, e.new);
            }
        }
        let selected: HashSet<String> = cs.services.iter().map(|s| s.name.clone()).collect();
        println!("commit message: {}", commit_message(&cs, &selected));
        std::fs::remove_dir_all(&root).ok();
    }
}
