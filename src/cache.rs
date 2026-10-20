//! SQLite cache layer for persisting poller data across restarts.
//! On startup, cached data is loaded into AppState for instant UI.
//! Pollers call cache methods after each successful poll cycle.

use crate::models::deployment::{DeploymentStatus, HelmReleaseInfo, PodInfo};
use crate::models::issue::{GitHubIssue, IssueComment};
use crate::models::release::{GitHubRelease, PullRequest, WorkflowRun};
use crate::pollers::vault::VaultState;
use crate::state::{DeclaredVersion, EnvironmentState, KustomizationInfo, OperatorPins};
use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// Classification of a repo in the org.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoKind {
    Service,
    Library,
    Action,
    Gitops,
    /// Not relevant to caravan (e.g. archived, docs-only)
    Other,
}

impl RepoKind {
    pub fn as_str(&self) -> &str {
        match self {
            RepoKind::Service => "service",
            RepoKind::Library => "library",
            RepoKind::Action => "action",
            RepoKind::Gitops => "gitops",
            RepoKind::Other => "other",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "service" => RepoKind::Service,
            "library" => RepoKind::Library,
            "action" => RepoKind::Action,
            "gitops" => RepoKind::Gitops,
            _ => RepoKind::Other,
        }
    }
}

/// A repo discovered from the GitHub org.
#[derive(Debug, Clone)]
pub struct OrgRepo {
    pub name: String,
    pub description: Option<String>,
    pub language: Option<String>,
    pub archived: bool,
    pub fork: bool,
    pub private: bool,
    pub license: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
    pub pushed_at: Option<DateTime<Utc>>,
    pub kind: RepoKind,
}

/// Thread-safe cache wrapping a SQLite connection.
pub struct Cache {
    conn: Mutex<Connection>,
}

impl Cache {
    /// Open (or create) the cache database.
    pub fn open() -> Result<Self> {
        let path = Self::db_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        tracing::info!("Opening cache at {}", path.display());
        let conn = Connection::open(&path)?;
        let cache = Self {
            conn: Mutex::new(conn),
        };
        cache.migrate()?;
        Ok(cache)
    }

    fn db_path() -> Result<PathBuf> {
        Ok(crate::config::data_dir().join("cache.db"))
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;

            CREATE TABLE IF NOT EXISTS helm_releases (
                env TEXT NOT NULL,
                name TEXT NOT NULL,
                namespace TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'Unknown',
                status_message TEXT,
                chart_version TEXT,
                image_tag TEXT,
                chart_name TEXT,
                last_attempted_revision TEXT,
                last_applied_revision TEXT,
                last_transition_time TEXT,
                suspended INTEGER NOT NULL DEFAULT 0,
                app_name_label TEXT,
                kustomize_name TEXT,
                PRIMARY KEY (env, name, namespace)
            );

            CREATE TABLE IF NOT EXISTS pods (
                env TEXT NOT NULL,
                name TEXT NOT NULL,
                namespace TEXT NOT NULL,
                phase TEXT NOT NULL,
                status TEXT NOT NULL,
                restarts INTEGER NOT NULL DEFAULT 0,
                created TEXT,
                ready INTEGER NOT NULL DEFAULT 0,
                ready_count TEXT NOT NULL DEFAULT '0/0',
                images TEXT NOT NULL DEFAULT '[]',
                node_name TEXT,
                reason TEXT,
                PRIMARY KEY (env, name, namespace)
            );

            CREATE TABLE IF NOT EXISTS workflow_runs (
                id INTEGER PRIMARY KEY,
                repo TEXT NOT NULL,
                workflow_name TEXT NOT NULL,
                workflow_file TEXT,
                run_number INTEGER NOT NULL,
                status TEXT NOT NULL,
                conclusion TEXT,
                head_branch TEXT,
                head_sha TEXT NOT NULL,
                event TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                html_url TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS pull_requests (
                repo TEXT NOT NULL,
                number INTEGER NOT NULL,
                title TEXT NOT NULL,
                state TEXT NOT NULL,
                draft INTEGER NOT NULL DEFAULT 0,
                author TEXT,
                head_branch TEXT NOT NULL,
                base_branch TEXT NOT NULL,
                head_sha TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                merged_at TEXT,
                html_url TEXT NOT NULL,
                labels TEXT NOT NULL DEFAULT '[]',
                review_comments INTEGER NOT NULL DEFAULT 0,
                mergeable INTEGER,
                is_gitops INTEGER NOT NULL DEFAULT 0,
                reviewers TEXT NOT NULL DEFAULT '[]',
                reviews TEXT NOT NULL DEFAULT '[]',
                PRIMARY KEY (repo, number)
            );

            CREATE TABLE IF NOT EXISTS releases (
                id INTEGER PRIMARY KEY,
                repo TEXT NOT NULL,
                tag TEXT NOT NULL,
                name TEXT,
                body TEXT,
                draft INTEGER NOT NULL DEFAULT 0,
                prerelease INTEGER NOT NULL DEFAULT 0,
                author TEXT,
                published_at TEXT,
                created_at TEXT NOT NULL,
                html_url TEXT NOT NULL,
                target_commitish TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS gitops_versions (
                service TEXT NOT NULL,
                env TEXT NOT NULL,
                chart_version TEXT,
                image_tag TEXT,
                PRIMARY KEY (service, env)
            );

            CREATE TABLE IF NOT EXISTS vault_status (
                env TEXT PRIMARY KEY,
                url TEXT NOT NULL,
                reachable INTEGER NOT NULL DEFAULT 0,
                sealed INTEGER,
                initialized INTEGER,
                cluster_name TEXT,
                version TEXT,
                error TEXT,
                last_poll TEXT
            );

            CREATE TABLE IF NOT EXISTS commit_comparisons (
                repo TEXT NOT NULL,
                base_env TEXT NOT NULL,
                head_env TEXT NOT NULL,
                ahead_by INTEGER NOT NULL,
                PRIMARY KEY (repo, base_env, head_env)
            );

            CREATE TABLE IF NOT EXISTS repo_tags (
                repo TEXT NOT NULL,
                position INTEGER NOT NULL,
                tag TEXT NOT NULL,
                sha TEXT NOT NULL,
                PRIMARY KEY (repo, position)
            );

            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS org_repos (
                name TEXT PRIMARY KEY,
                description TEXT,
                language TEXT,
                archived INTEGER NOT NULL DEFAULT 0,
                fork INTEGER NOT NULL DEFAULT 0,
                private INTEGER NOT NULL DEFAULT 1,
                license TEXT,
                updated_at TEXT,
                repo_kind TEXT NOT NULL DEFAULT 'library'
            );

            CREATE TABLE IF NOT EXISTS hidden_repos (
                name TEXT PRIMARY KEY
            );

            CREATE TABLE IF NOT EXISTS http_cache (
                route TEXT PRIMARY KEY,
                etag TEXT NOT NULL,
                body TEXT NOT NULL,
                fetched_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS issues (
                repo TEXT NOT NULL,
                number INTEGER NOT NULL,
                title TEXT NOT NULL,
                state TEXT NOT NULL,
                author TEXT,
                assignees TEXT NOT NULL DEFAULT '[]',
                labels TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                closed_at TEXT,
                html_url TEXT NOT NULL,
                body TEXT,
                milestone TEXT,
                is_epic INTEGER NOT NULL DEFAULT 0,
                epic_progress_checked INTEGER,
                epic_progress_total INTEGER,
                matched_repos TEXT NOT NULL DEFAULT '[]',
                project_status TEXT,
                project_priority TEXT,
                on_project INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (repo, number)
            );

            CREATE TABLE IF NOT EXISTS issue_comments (
                repo TEXT NOT NULL,
                issue_number INTEGER NOT NULL,
                id INTEGER NOT NULL,
                author TEXT,
                body TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                html_url TEXT NOT NULL,
                PRIMARY KEY (repo, issue_number, id)
            );

            CREATE TABLE IF NOT EXISTS issue_images (
                url TEXT NOT NULL PRIMARY KEY,
                data BLOB NOT NULL,
                fetched_at TEXT NOT NULL
            );
        ",
        )?;

        // Migrations for existing databases
        let _ = conn.execute_batch(
            "
            ALTER TABLE org_repos ADD COLUMN private INTEGER NOT NULL DEFAULT 1;
            ALTER TABLE org_repos ADD COLUMN license TEXT;
        ",
        );

        let _ = conn.execute_batch(
            "
            ALTER TABLE pull_requests ADD COLUMN reviewers TEXT NOT NULL DEFAULT '[]';
            ALTER TABLE pull_requests ADD COLUMN reviews TEXT NOT NULL DEFAULT '[]';
        ",
        );

        let _ = conn.execute_batch(
            "
            ALTER TABLE org_repos ADD COLUMN pushed_at TEXT;
        ",
        );

        // Each ALTER fails on its own once the column exists, so they run
        // one at a time.
        for stmt in [
            "ALTER TABLE helm_releases ADD COLUMN ready INTEGER",
            "ALTER TABLE helm_releases ADD COLUMN reconciling INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE helm_releases ADD COLUMN generation INTEGER",
            "ALTER TABLE helm_releases ADD COLUMN observed_generation INTEGER",
            "ALTER TABLE helm_releases ADD COLUMN replica_count INTEGER",
            "ALTER TABLE helm_releases ADD COLUMN values_images TEXT NOT NULL DEFAULT '{}'",
            "ALTER TABLE pods ADD COLUMN containers TEXT NOT NULL DEFAULT '[]'",
            "ALTER TABLE pods ADD COLUMN volume_images TEXT NOT NULL DEFAULT '[]'",
            "ALTER TABLE vault_status ADD COLUMN sealed_since TEXT",
        ] {
            let _ = conn.execute(stmt, []);
        }

        // Drop conditional-request entries whose route hasn't been fetched in a
        // week (e.g. time-windowed search queries that will never recur).
        let cutoff = (Utc::now() - chrono::Duration::days(7)).to_rfc3339();
        let _ = conn.execute(
            "DELETE FROM http_cache WHERE fetched_at < ?1",
            params![cutoff],
        );

        Ok(())
    }

    // ── HTTP conditional-request cache ─────────────────────────────────────

    /// Look up the stored (etag, body) for a route.
    pub fn get_http_cache(&self, route: &str) -> Result<Option<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT etag, body FROM http_cache WHERE route = ?1")?;
        let mut rows = stmt.query(params![route])?;
        match rows.next()? {
            Some(row) => Ok(Some((row.get(0)?, row.get(1)?))),
            None => Ok(None),
        }
    }

    pub fn put_http_cache(&self, route: &str, etag: &str, body: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO http_cache (route, etag, body, fetched_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(route) DO UPDATE SET etag = ?2, body = ?3, fetched_at = ?4",
            params![route, etag, body, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Refresh a cache entry's fetched_at on a 304 so live routes survive pruning.
    pub fn touch_http_cache(&self, route: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE http_cache SET fetched_at = ?2 WHERE route = ?1",
            params![route, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    // ── Write methods (called by pollers) ──────────────────────────────────

    pub fn save_environment(
        &self,
        env_name: &str,
        hrs: &[HelmReleaseInfo],
        pods: &[PodInfo],
        kustomization: Option<&KustomizationInfo>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;

        tx.execute(
            "DELETE FROM helm_releases WHERE env = ?1",
            params![env_name],
        )?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO helm_releases (env, name, namespace, status, status_message, chart_version, image_tag, chart_name, last_attempted_revision, last_applied_revision, last_transition_time, suspended, app_name_label, kustomize_name, ready, reconciling, generation, observed_generation, replica_count, values_images)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)"
            )?;
            for hr in hrs {
                stmt.execute(params![
                    env_name,
                    hr.name,
                    hr.namespace,
                    format!("{:?}", hr.status),
                    hr.status_message,
                    hr.chart_version,
                    hr.image_tag,
                    hr.chart_name,
                    hr.last_attempted_revision,
                    hr.last_applied_revision,
                    hr.last_transition_time.map(|t| t.to_rfc3339()),
                    hr.suspended as i32,
                    hr.app_name_label,
                    hr.kustomize_name,
                    hr.ready.map(|r| r as i32),
                    hr.reconciling as i32,
                    hr.generation,
                    hr.observed_generation,
                    hr.replica_count,
                    serde_json::to_string(&hr.values_images).unwrap_or_default(),
                ])?;
            }
        }

        tx.execute("DELETE FROM pods WHERE env = ?1", params![env_name])?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO pods (env, name, namespace, phase, status, restarts, created, ready, ready_count, images, node_name, reason, containers, volume_images)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
            )?;
            for pod in pods {
                stmt.execute(params![
                    env_name,
                    pod.name,
                    pod.namespace,
                    pod.phase,
                    pod.status,
                    pod.restarts,
                    pod.created.map(|t| t.to_rfc3339()),
                    pod.ready as i32,
                    pod.ready_count,
                    serde_json::to_string(&pod.images).unwrap_or_default(),
                    pod.node_name,
                    pod.reason,
                    serde_json::to_string(&pod.containers).unwrap_or_default(),
                    serde_json::to_string(&pod.volume_images).unwrap_or_default(),
                ])?;
            }
        }

        if let Some(k) = kustomization {
            tx.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
                params![
                    format!("kustomization:{}", env_name),
                    serde_json::to_string(k).unwrap_or_default()
                ],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    pub fn save_github(
        &self,
        workflow_runs: &[WorkflowRun],
        pull_requests: &[PullRequest],
        releases: &[GitHubRelease],
        open_gitops_prs: &[PullRequest],
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;

        tx.execute("DELETE FROM workflow_runs", [])?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO workflow_runs (id, repo, workflow_name, workflow_file, run_number, status, conclusion, head_branch, head_sha, event, created_at, updated_at, html_url)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
            )?;
            for r in workflow_runs {
                stmt.execute(params![
                    r.id as i64,
                    r.repo,
                    r.workflow_name,
                    r.workflow_file,
                    r.run_number as i64,
                    r.status,
                    r.conclusion,
                    r.head_branch,
                    r.head_sha,
                    r.event,
                    r.created_at.to_rfc3339(),
                    r.updated_at.to_rfc3339(),
                    r.html_url,
                ])?;
            }
        }

        tx.execute("DELETE FROM pull_requests", [])?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO pull_requests (repo, number, title, state, draft, author, head_branch, base_branch, head_sha, created_at, updated_at, merged_at, html_url, labels, review_comments, mergeable, is_gitops, reviewers, reviews)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)"
            )?;
            for pr in pull_requests {
                stmt.execute(params![
                    pr.repo,
                    pr.number as i64,
                    pr.title,
                    pr.state,
                    pr.draft as i32,
                    pr.author,
                    pr.head_branch,
                    pr.base_branch,
                    pr.head_sha,
                    pr.created_at.to_rfc3339(),
                    pr.updated_at.to_rfc3339(),
                    pr.merged_at.map(|t| t.to_rfc3339()),
                    pr.html_url,
                    serde_json::to_string(&pr.labels).unwrap_or_default(),
                    pr.review_comments as i64,
                    pr.mergeable.map(|b| b as i32),
                    0i32,
                    serde_json::to_string(&pr.reviewers).unwrap_or_default(),
                    serde_json::to_string(&pr.reviews).unwrap_or_default(),
                ])?;
            }
            for pr in open_gitops_prs {
                stmt.execute(params![
                    pr.repo,
                    pr.number as i64,
                    pr.title,
                    pr.state,
                    pr.draft as i32,
                    pr.author,
                    pr.head_branch,
                    pr.base_branch,
                    pr.head_sha,
                    pr.created_at.to_rfc3339(),
                    pr.updated_at.to_rfc3339(),
                    pr.merged_at.map(|t| t.to_rfc3339()),
                    pr.html_url,
                    serde_json::to_string(&pr.labels).unwrap_or_default(),
                    pr.review_comments as i64,
                    pr.mergeable.map(|b| b as i32),
                    1i32,
                    serde_json::to_string(&pr.reviewers).unwrap_or_default(),
                    serde_json::to_string(&pr.reviews).unwrap_or_default(),
                ])?;
            }
        }

        tx.execute("DELETE FROM releases", [])?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO releases (id, repo, tag, name, body, draft, prerelease, author, published_at, created_at, html_url, target_commitish)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
            )?;
            for r in releases {
                stmt.execute(params![
                    r.id as i64,
                    r.repo,
                    r.tag,
                    r.name,
                    r.body,
                    r.draft as i32,
                    r.prerelease as i32,
                    r.author,
                    r.published_at.map(|t| t.to_rfc3339()),
                    r.created_at.to_rfc3339(),
                    r.html_url,
                    r.target_commitish,
                ])?;
            }
        }

        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('last_github_poll', ?1)",
            params![Utc::now().to_rfc3339()],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Persist one gitops poll as JSON blobs in the meta table.
    pub fn save_gitops_snapshot(
        &self,
        services: &[String],
        versions: &HashMap<String, HashMap<String, DeclaredVersion>>,
        operator_pins: &HashMap<String, OperatorPins>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        for (key, value) in [
            ("gitops_services", serde_json::to_string(services)?),
            ("gitops_declared_versions", serde_json::to_string(versions)?),
            ("operator_pins", serde_json::to_string(operator_pins)?),
        ] {
            tx.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
                params![key, value],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn save_vault_status(&self, env: &str, vs: &VaultState) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO vault_status (env, url, reachable, sealed, initialized, cluster_name, version, error, last_poll, sealed_since)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                env,
                vs.url,
                vs.reachable as i32,
                vs.seal_status.as_ref().map(|s| s.sealed as i32),
                vs.seal_status.as_ref().map(|s| s.initialized as i32),
                vs.seal_status.as_ref().map(|s| s.cluster_name.as_str()),
                vs.seal_status.as_ref().map(|s| s.version.as_str()),
                vs.error,
                vs.last_poll.map(|t| t.to_rfc3339()),
                vs.sealed_since.map(|t| t.to_rfc3339()),
            ],
        )?;
        Ok(())
    }

    /// When each env's vault was first seen sealed, for envs still sealed at
    /// the last save. Lets a restart keep the "sealed since" clock.
    pub fn load_vault_sealed_since(&self) -> Result<HashMap<String, DateTime<Utc>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT env, sealed_since FROM vault_status WHERE sealed_since IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (env, since) = row?;
            if let Ok(t) = DateTime::parse_from_rfc3339(&since) {
                out.insert(env, t.with_timezone(&Utc));
            }
        }
        Ok(out)
    }

    pub fn save_commit_comparisons(
        &self,
        comparisons: &HashMap<(String, String, String), i64>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM commit_comparisons", [])?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO commit_comparisons (repo, base_env, head_env, ahead_by) VALUES (?1, ?2, ?3, ?4)"
            )?;
            for ((repo, base, head), ahead) in comparisons {
                stmt.execute(params![repo, base, head, ahead])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn save_commit_relations(
        &self,
        relations: &HashMap<(String, String, String), crate::state::CommitRelation>,
    ) -> Result<()> {
        let rows: Vec<(&(String, String, String), &crate::state::CommitRelation)> =
            relations.iter().collect();
        self.set_meta("commit_relations", &serde_json::to_string(&rows)?)
    }

    pub fn load_commit_relations(
        &self,
    ) -> Result<HashMap<(String, String, String), crate::state::CommitRelation>> {
        let Some(json) = self.get_meta("commit_relations")? else {
            return Ok(HashMap::new());
        };
        let rows: Vec<((String, String, String), crate::state::CommitRelation)> =
            serde_json::from_str(&json)?;
        Ok(rows.into_iter().collect())
    }

    pub fn save_pr_builds(
        &self,
        builds: &HashMap<(String, u64), crate::state::PrBuild>,
    ) -> Result<()> {
        let rows: Vec<(&(String, u64), &crate::state::PrBuild)> = builds.iter().collect();
        self.set_meta("pr_builds", &serde_json::to_string(&rows)?)
    }

    pub fn load_pr_builds(&self) -> Result<HashMap<(String, u64), crate::state::PrBuild>> {
        let Some(json) = self.get_meta("pr_builds")? else {
            return Ok(HashMap::new());
        };
        let rows: Vec<((String, u64), crate::state::PrBuild)> = serde_json::from_str(&json)?;
        Ok(rows.into_iter().collect())
    }

    pub fn save_repo_tags(&self, tags: &HashMap<String, Vec<(String, String)>>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM repo_tags", [])?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO repo_tags (repo, position, tag, sha) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (repo, tag_list) in tags {
                for (position, (tag, sha)) in tag_list.iter().enumerate() {
                    stmt.execute(params![repo, position as i64, tag, sha])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load_repo_tags(&self) -> Result<HashMap<String, Vec<(String, String)>>> {
        let conn = self.conn.lock().unwrap();
        let mut map: HashMap<String, Vec<(String, String)>> = HashMap::new();
        let mut stmt =
            conn.prepare("SELECT repo, tag, sha FROM repo_tags ORDER BY repo, position")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (repo, tag, sha) = row?;
            map.entry(repo).or_default().push((tag, sha));
        }
        Ok(map)
    }

    pub fn save_org_repos(&self, repos: &[OrgRepo]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM org_repos", [])?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO org_repos (name, description, language, archived, fork, private, license, updated_at, pushed_at, repo_kind) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
            )?;
            for r in repos {
                stmt.execute(params![
                    r.name,
                    r.description,
                    r.language,
                    r.archived as i32,
                    r.fork as i32,
                    r.private as i32,
                    r.license,
                    r.updated_at.map(|t| t.to_rfc3339()),
                    r.pushed_at.map(|t| t.to_rfc3339()),
                    r.kind.as_str(),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn save_hidden_repo(&self, name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO hidden_repos (name) VALUES (?1)",
            params![name],
        )?;
        Ok(())
    }

    pub fn remove_hidden_repo(&self, name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM hidden_repos WHERE name = ?1", params![name])?;
        Ok(())
    }

    pub fn load_hidden_repos(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.conn.lock().unwrap();
        let mut set = std::collections::HashSet::new();
        let mut stmt = conn.prepare("SELECT name FROM hidden_repos")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            set.insert(row?);
        }
        Ok(set)
    }

    // ── Issues ────────────────────────────────────────────────────────────

    pub fn save_issues(&self, issues: &[GitHubIssue]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;

        tx.execute("DELETE FROM issues", [])?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO issues (repo, number, title, state, author, assignees, labels, created_at, updated_at, closed_at, html_url, body, milestone, is_epic, epic_progress_checked, epic_progress_total, matched_repos, project_status, project_priority, on_project)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)"
            )?;
            for i in issues {
                let (checked, total) = i.epic_progress.unzip();
                stmt.execute(params![
                    i.repo,
                    i.number as i64,
                    i.title,
                    i.state,
                    i.author,
                    serde_json::to_string(&i.assignees).unwrap_or_default(),
                    serde_json::to_string(&i.labels).unwrap_or_default(),
                    i.created_at.to_rfc3339(),
                    i.updated_at.to_rfc3339(),
                    i.closed_at.map(|t| t.to_rfc3339()),
                    i.html_url,
                    i.body,
                    i.milestone,
                    i.is_epic as i32,
                    checked.map(|c| c as i64),
                    total.map(|t| t as i64),
                    serde_json::to_string(&i.matched_repos).unwrap_or_default(),
                    i.project_status,
                    i.project_priority,
                    i.on_project as i32,
                ])?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    pub fn load_issues(&self) -> Result<Vec<GitHubIssue>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT repo, number, title, state, author, assignees, labels, created_at, updated_at, closed_at, html_url, body, milestone, is_epic, epic_progress_checked, epic_progress_total, matched_repos, project_status, project_priority, on_project FROM issues"
        )?;
        let rows = stmt.query_map([], |row| {
            let assignees_json: String = row.get(5)?;
            let labels_json: String = row.get(6)?;
            let matched_repos_json: String = row.get(16)?;
            let checked: Option<i64> = row.get(14)?;
            let total: Option<i64> = row.get(15)?;
            let epic_progress = match (checked, total) {
                (Some(c), Some(t)) => Some((c as u32, t as u32)),
                _ => None,
            };
            let closed_at_str: Option<String> = row.get(9)?;

            Ok(GitHubIssue {
                repo: row.get(0)?,
                number: row.get::<_, i64>(1)? as u64,
                title: row.get(2)?,
                state: row.get(3)?,
                author: row.get(4)?,
                assignees: serde_json::from_str(&assignees_json).unwrap_or_default(),
                labels: serde_json::from_str(&labels_json).unwrap_or_default(),
                created_at: parse_datetime_required(row.get::<_, String>(7)?),
                updated_at: parse_datetime_required(row.get::<_, String>(8)?),
                closed_at: closed_at_str.as_deref().and_then(parse_datetime),
                html_url: row.get(10)?,
                body: row.get(11)?,
                milestone: row.get(12)?,
                is_epic: row.get::<_, i32>(13)? != 0,
                epic_progress,
                matched_repos: serde_json::from_str(&matched_repos_json).unwrap_or_default(),
                project_status: row.get(17)?,
                project_priority: row.get(18)?,
                on_project: row.get::<_, i32>(19)? != 0,
            })
        })?;

        let mut issues = Vec::new();
        for row in rows {
            issues.push(row?);
        }
        Ok(issues)
    }

    // ── Meta key-value store ─────────────────────────────────────────────

    // ── Issue comments ───────────────────────────────────────────────────

    pub fn save_comments(
        &self,
        repo: &str,
        issue_number: u64,
        comments: &[IssueComment],
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM issue_comments WHERE repo = ?1 AND issue_number = ?2",
            params![repo, issue_number as i64],
        )?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO issue_comments (repo, issue_number, id, author, body, created_at, updated_at, html_url)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for c in comments {
                stmt.execute(params![
                    repo,
                    issue_number as i64,
                    c.id as i64,
                    c.author,
                    c.body,
                    c.created_at.to_rfc3339(),
                    c.updated_at.to_rfc3339(),
                    c.html_url,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load_comments(&self, repo: &str, issue_number: u64) -> Result<Vec<IssueComment>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, author, body, created_at, updated_at, html_url
             FROM issue_comments WHERE repo = ?1 AND issue_number = ?2
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![repo, issue_number as i64], |row| {
            Ok(IssueComment {
                id: row.get::<_, i64>(0)? as u64,
                author: row.get(1)?,
                body: row.get(2)?,
                created_at: parse_datetime_required(row.get::<_, String>(3)?),
                updated_at: parse_datetime_required(row.get::<_, String>(4)?),
                html_url: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ── Issue images ─────────────────────────────────────────────────────

    pub fn save_image(&self, url: &str, data: &[u8]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO issue_images (url, data, fetched_at) VALUES (?1, ?2, ?3)",
            params![url, data, chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn load_image(&self, url: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM issue_images WHERE url = ?1")?;
        let result = stmt.query_row(params![url], |row| row.get::<_, Vec<u8>>(0));
        match result {
            Ok(data) => Ok(Some(data)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn load_all_images(&self) -> Result<HashMap<String, Vec<u8>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT url, data FROM issue_images")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (url, data) = row?;
            map.insert(url, data);
        }
        Ok(map)
    }

    // ── Meta key-value store ─────────────────────────────────────────────

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
        let result = stmt.query_row(params![key], |row| row.get::<_, String>(0));
        match result {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    // ── Read methods (called on startup) ───────────────────────────────────

    pub fn load_environments(&self) -> Result<HashMap<String, EnvironmentState>> {
        let conn = self.conn.lock().unwrap();
        let mut envs: HashMap<String, EnvironmentState> = HashMap::new();

        // Load helm releases
        let mut stmt = conn.prepare("SELECT env, name, namespace, status, status_message, chart_version, image_tag, chart_name, last_attempted_revision, last_applied_revision, last_transition_time, suspended, app_name_label, kustomize_name, ready, reconciling, generation, observed_generation, replica_count, values_images FROM helm_releases")?;
        let rows = stmt.query_map([], |row| {
            let env: String = row.get(0)?;
            let status_str: String = row.get(3)?;
            let status = parse_deployment_status(&status_str);
            let last_transition: Option<String> = row.get(10)?;
            let values_images_json: String = row.get(19)?;
            Ok((
                env,
                HelmReleaseInfo {
                    name: row.get(1)?,
                    namespace: row.get(2)?,
                    status,
                    status_message: row.get(4)?,
                    chart_version: row.get(5)?,
                    image_tag: row.get(6)?,
                    chart_name: row.get(7)?,
                    last_attempted_revision: row.get(8)?,
                    last_applied_revision: row.get(9)?,
                    last_transition_time: last_transition.and_then(|s| parse_datetime(&s)),
                    suspended: row.get::<_, i32>(11)? != 0,
                    app_name_label: row.get(12)?,
                    kustomize_name: row.get(13)?,
                    ready: row.get::<_, Option<i32>>(14)?.map(|r| r != 0),
                    reconciling: row.get::<_, i32>(15)? != 0,
                    generation: row.get(16)?,
                    observed_generation: row.get(17)?,
                    replica_count: row.get(18)?,
                    values_images: serde_json::from_str(&values_images_json).unwrap_or_default(),
                },
            ))
        })?;
        for row in rows {
            let (env, hr) = row?;
            envs.entry(env).or_default().helm_releases.push(hr);
        }

        // Load pods
        let mut stmt = conn.prepare("SELECT env, name, namespace, phase, status, restarts, created, ready, ready_count, images, node_name, reason, containers, volume_images FROM pods")?;
        let rows = stmt.query_map([], |row| {
            let env: String = row.get(0)?;
            let created_str: Option<String> = row.get(6)?;
            let created = created_str.and_then(|s| parse_datetime(&s));
            let images_json: String = row.get(9)?;
            let images: Vec<String> = serde_json::from_str(&images_json).unwrap_or_default();
            let containers_json: String = row.get(12)?;
            let volume_images_json: String = row.get(13)?;
            Ok((
                env,
                PodInfo {
                    name: row.get(1)?,
                    namespace: row.get(2)?,
                    phase: row.get(3)?,
                    status: row.get(4)?,
                    restarts: row.get(5)?,
                    created,
                    started_at: created,
                    ready: row.get::<_, i32>(7)? != 0,
                    ready_count: row.get(8)?,
                    images,
                    node_name: row.get(10)?,
                    reason: row.get(11)?,
                    cpu_request_millicores: None,
                    cpu_limit_millicores: None,
                    memory_request_bytes: None,
                    memory_limit_bytes: None,
                    containers: serde_json::from_str(&containers_json).unwrap_or_default(),
                    volume_images: serde_json::from_str(&volume_images_json).unwrap_or_default(),
                },
            ))
        })?;
        for row in rows {
            let (env, pod) = row?;
            envs.entry(env).or_default().pods.push(pod);
        }

        // Mark all cached envs as connected (they were when cached)
        for env in envs.values_mut() {
            env.connected = true;
        }

        let mut stmt =
            conn.prepare("SELECT key, value FROM meta WHERE key LIKE 'kustomization:%'")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (key, value) = row?;
            let env = key.trim_start_matches("kustomization:");
            if let Some(env_state) = envs.get_mut(env) {
                env_state.kustomization = serde_json::from_str(&value).ok();
            }
        }

        Ok(envs)
    }

    #[allow(clippy::type_complexity)]
    pub fn load_github(
        &self,
    ) -> Result<(
        Vec<WorkflowRun>,
        Vec<PullRequest>,
        Vec<GitHubRelease>,
        Vec<PullRequest>,
    )> {
        let conn = self.conn.lock().unwrap();

        let mut workflow_runs = Vec::new();
        {
            let mut stmt = conn.prepare("SELECT id, repo, workflow_name, workflow_file, run_number, status, conclusion, head_branch, head_sha, event, created_at, updated_at, html_url FROM workflow_runs")?;
            let rows = stmt.query_map([], |row| {
                Ok(WorkflowRun {
                    id: row.get::<_, i64>(0)? as u64,
                    repo: row.get(1)?,
                    workflow_name: row.get(2)?,
                    workflow_file: row.get(3)?,
                    run_number: row.get::<_, i64>(4)? as u64,
                    status: row.get(5)?,
                    conclusion: row.get(6)?,
                    head_branch: row.get(7)?,
                    head_sha: row.get(8)?,
                    event: row.get(9)?,
                    created_at: parse_datetime_required(row.get::<_, String>(10)?),
                    updated_at: parse_datetime_required(row.get::<_, String>(11)?),
                    html_url: row.get(12)?,
                })
            })?;
            for row in rows {
                workflow_runs.push(row?);
            }
        }

        let mut pull_requests = Vec::new();
        let mut gitops_prs = Vec::new();
        {
            let mut stmt = conn.prepare("SELECT repo, number, title, state, draft, author, head_branch, base_branch, head_sha, created_at, updated_at, merged_at, html_url, labels, review_comments, mergeable, is_gitops, reviewers, reviews FROM pull_requests")?;
            let rows = stmt.query_map([], |row| {
                let labels_json: String = row.get(13)?;
                let labels: Vec<String> = serde_json::from_str(&labels_json).unwrap_or_default();
                let merged_str: Option<String> = row.get(11)?;
                let mergeable_int: Option<i32> = row.get(15)?;
                Ok((
                    PullRequest {
                        repo: row.get(0)?,
                        number: row.get::<_, i64>(1)? as u64,
                        title: row.get(2)?,
                        state: row.get(3)?,
                        draft: row.get::<_, i32>(4)? != 0,
                        author: row.get(5)?,
                        head_branch: row.get(6)?,
                        base_branch: row.get(7)?,
                        head_sha: row.get(8)?,
                        created_at: parse_datetime_required(row.get::<_, String>(9)?),
                        updated_at: parse_datetime_required(row.get::<_, String>(10)?),
                        merged_at: merged_str.and_then(|s| parse_datetime(&s)),
                        html_url: row.get(12)?,
                        labels,
                        review_comments: row.get::<_, i64>(14)? as u64,
                        mergeable: mergeable_int.map(|i| i != 0),
                        reviewers: {
                            let s: String = row.get(17)?;
                            serde_json::from_str(&s).unwrap_or_default()
                        },
                        reviews: {
                            let s: String = row.get(18)?;
                            serde_json::from_str(&s).unwrap_or_default()
                        },
                    },
                    row.get::<_, i32>(16)?,
                ))
            })?;
            for row in rows {
                let (pr, is_gitops) = row?;
                if is_gitops != 0 {
                    gitops_prs.push(pr);
                } else {
                    pull_requests.push(pr);
                }
            }
        }

        let mut releases = Vec::new();
        {
            let mut stmt = conn.prepare("SELECT id, repo, tag, name, body, draft, prerelease, author, published_at, created_at, html_url, target_commitish FROM releases")?;
            let rows = stmt.query_map([], |row| {
                let published_str: Option<String> = row.get(8)?;
                Ok(GitHubRelease {
                    id: row.get::<_, i64>(0)? as u64,
                    repo: row.get(1)?,
                    tag: row.get(2)?,
                    name: row.get(3)?,
                    body: row.get(4)?,
                    draft: row.get::<_, i32>(5)? != 0,
                    prerelease: row.get::<_, i32>(6)? != 0,
                    author: row.get(7)?,
                    published_at: published_str.and_then(|s| parse_datetime(&s)),
                    created_at: parse_datetime_required(row.get::<_, String>(9)?),
                    html_url: row.get(10)?,
                    target_commitish: row.get(11)?,
                })
            })?;
            for row in rows {
                releases.push(row?);
            }
        }

        Ok((workflow_runs, pull_requests, releases, gitops_prs))
    }

    /// The last gitops snapshot: (services, declared versions, operator pins).
    /// Empty when nothing has been cached yet.
    #[allow(clippy::type_complexity)]
    pub fn load_gitops_snapshot(
        &self,
    ) -> Result<(
        Vec<String>,
        HashMap<String, HashMap<String, DeclaredVersion>>,
        HashMap<String, OperatorPins>,
    )> {
        let services = self
            .get_meta("gitops_services")?
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default();
        let versions = self
            .get_meta("gitops_declared_versions")?
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default();
        let pins = self
            .get_meta("operator_pins")?
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default();
        Ok((services, versions, pins))
    }

    pub fn load_commit_comparisons(&self) -> Result<HashMap<(String, String, String), i64>> {
        let conn = self.conn.lock().unwrap();
        let mut map = HashMap::new();
        let mut stmt =
            conn.prepare("SELECT repo, base_env, head_env, ahead_by FROM commit_comparisons")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows {
            let (repo, base, head, ahead) = row?;
            map.insert((repo, base, head), ahead);
        }
        Ok(map)
    }

    pub fn load_org_repos(&self) -> Result<Vec<OrgRepo>> {
        let conn = self.conn.lock().unwrap();
        let mut repos = Vec::new();
        let mut stmt = conn.prepare("SELECT name, description, language, archived, fork, private, license, updated_at, pushed_at, repo_kind FROM org_repos")?;
        let rows = stmt.query_map([], |row| {
            let updated_str: Option<String> = row.get(7)?;
            let pushed_str: Option<String> = row.get(8)?;
            Ok(OrgRepo {
                name: row.get(0)?,
                description: row.get(1)?,
                language: row.get(2)?,
                archived: row.get::<_, i32>(3)? != 0,
                fork: row.get::<_, i32>(4)? != 0,
                private: row.get::<_, i32>(5).unwrap_or(1) != 0,
                license: row.get(6)?,
                updated_at: updated_str.and_then(|s| parse_datetime(&s)),
                pushed_at: pushed_str.and_then(|s| parse_datetime(&s)),
                kind: RepoKind::from_str(&row.get::<_, String>(9)?),
            })
        })?;
        for row in rows {
            repos.push(row?);
        }
        Ok(repos)
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn parse_datetime(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn parse_datetime_required(s: String) -> DateTime<Utc> {
    parse_datetime(&s).unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap())
}

fn parse_deployment_status(s: &str) -> DeploymentStatus {
    match s {
        "Healthy" => DeploymentStatus::Healthy,
        "Progressing" => DeploymentStatus::Progressing,
        "Degraded" => DeploymentStatus::Degraded,
        "Failed" => DeploymentStatus::Failed,
        "Suspended" => DeploymentStatus::Suspended,
        "NotDeployed" => DeploymentStatus::NotDeployed,
        _ => DeploymentStatus::Unknown,
    }
}
