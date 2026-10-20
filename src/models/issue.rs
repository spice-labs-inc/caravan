use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A GitHub Issue from any tracked repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubIssue {
    /// Repository name (e.g. "notes-api", "internal-docs")
    pub repo: String,
    /// Issue number
    pub number: u64,
    /// Issue title
    pub title: String,
    /// Issue state: "open" or "closed"
    pub state: String,
    /// Who created the issue (GitHub login)
    pub author: Option<String>,
    /// Assigned users (GitHub logins)
    pub assignees: Vec<String>,
    /// Labels on this issue
    pub labels: Vec<IssueLabel>,
    /// When the issue was created
    pub created_at: DateTime<Utc>,
    /// When the issue was last updated
    pub updated_at: DateTime<Utc>,
    /// When the issue was closed (if closed)
    pub closed_at: Option<DateTime<Utc>>,
    /// GitHub URL for this issue
    pub html_url: String,
    /// Issue body (markdown)
    pub body: Option<String>,
    /// Milestone name (if any)
    pub milestone: Option<String>,
    /// Whether this issue is an epic (title starts with "EPIC :" or "EPIC:")
    pub is_epic: bool,
    /// Epic progress: (checked, total) from task-list checkboxes in body
    pub epic_progress: Option<(u32, u32)>,
    /// For internal-docs issues: repo names mentioned in title+body
    pub matched_repos: Vec<String>,
    /// Status from GitHub Projects V2 (if available)
    pub project_status: Option<String>,
    /// Priority from GitHub Projects V2 (if available)
    pub project_priority: Option<String>,
    /// Whether this issue is on the Engineering Board (Projects V2)
    pub on_project: bool,
}

/// A label on a GitHub issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueLabel {
    pub name: String,
    pub color: String,
}

/// A comment on a GitHub issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueComment {
    pub id: u64,
    pub author: Option<String>,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub html_url: String,
}

impl GitHubIssue {
    /// Whether this issue is open
    pub fn is_open(&self) -> bool {
        self.state == "open"
    }

    /// Detect if this issue is an epic based on its title
    pub fn detect_epic(title: &str) -> bool {
        let t = title.trim();
        t.starts_with("EPIC :") || t.starts_with("EPIC:")
    }

    /// Parse task-list progress from issue body.
    /// Returns (checked_count, total_count) or None if no task-list items found.
    pub fn parse_task_list_progress(body: Option<&str>) -> Option<(u32, u32)> {
        let body = body?;
        let mut checked: u32 = 0;
        let mut total: u32 = 0;

        for line in body.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("- [x]") || trimmed.starts_with("- [X]") {
                checked += 1;
                total += 1;
            } else if trimmed.starts_with("- [ ]") {
                total += 1;
            }
        }

        if total > 0 {
            Some((checked, total))
        } else {
            None
        }
    }

    /// Match repo names mentioned in an internal-docs issue's title+body.
    /// Handles case-insensitive matching and common variations:
    /// - "Goat Rodeo" → "goatrodeo"
    /// - "notes-cli" → "notes-cli"
    /// - "Notes-Api" → "notes-api"
    pub fn match_repos(title: &str, body: Option<&str>, repo_names: &[String]) -> Vec<String> {
        let search_text = format!("{} {}", title, body.unwrap_or("")).to_lowercase();

        repo_names
            .iter()
            .filter(|name| {
                let lower = name.to_lowercase();
                // Direct match (e.g. "notes-api", "notes-cli")
                if search_text.contains(&lower) {
                    return true;
                }
                // Match with spaces instead of hyphens (e.g. "notes api")
                let spaced = lower.replace('-', " ");
                if spaced != lower && search_text.contains(&spaced) {
                    return true;
                }
                // Match "goatrodeo" → "goat rodeo" (insert space at camelCase boundaries)
                // Also match compound names split apart
                let with_spaces = split_compound_name(&lower);
                if with_spaces != lower && search_text.contains(&with_spaces) {
                    return true;
                }
                false
            })
            .cloned()
            .collect()
    }
}

/// Split a compound repo name into space-separated words.
/// "goatrodeo" → "goat rodeo" (known mapping)
/// For general cases, just returns the name unchanged.
fn split_compound_name(name: &str) -> String {
    // Known compound names in the org
    match name {
        "goatrodeo" => "goat rodeo".to_string(),
        _ => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_epic() {
        assert!(GitHubIssue::detect_epic("EPIC : User Authentication"));
        assert!(GitHubIssue::detect_epic("EPIC:User Authentication"));
        assert!(!GitHubIssue::detect_epic("Epic task"));
        assert!(!GitHubIssue::detect_epic("Not an epic"));
    }

    #[test]
    fn test_parse_task_list_progress() {
        let body = "Some text\n- [x] Done item\n- [ ] Todo item\n- [X] Also done\nOther text";
        assert_eq!(
            GitHubIssue::parse_task_list_progress(Some(body)),
            Some((2, 3))
        );

        assert_eq!(
            GitHubIssue::parse_task_list_progress(Some("No tasks")),
            None
        );
        assert_eq!(GitHubIssue::parse_task_list_progress(None), None);
    }

    #[test]
    fn test_match_repos() {
        let repos = vec![
            "engine".to_string(),
            "goatrodeo".to_string(),
            "notes-api".to_string(),
            "notes-cli".to_string(),
        ];

        let matched = GitHubIssue::match_repos("Engine: add auth", None, &repos);
        assert!(matched.contains(&"engine".to_string()));

        let matched = GitHubIssue::match_repos("Goat Rodeo: add detector", None, &repos);
        assert!(matched.contains(&"goatrodeo".to_string()));

        let matched = GitHubIssue::match_repos("Fix bug", Some("This affects notes-cli"), &repos);
        assert!(matched.contains(&"notes-cli".to_string()));
    }
}
