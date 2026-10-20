use crate::cache::Cache;
use crate::models::issue::{GitHubIssue, IssueComment};
use crate::pollers::gh_http::GhClient;
use crate::state::AppState;
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

use crate::ui::charts::compress_bare_urls;
use crate::ui::filters::GlobalFilters;
use crate::ui::icons;
use crate::ui::sidebar::View;
use crate::ui::theme::{self, Colors, ThemeMode};
use crate::ui::widgets;
use eframe::egui;
use regex::Regex;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Which sub-tab is active on the Issues page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum IssuesTab {
    #[default]
    ByRepo,
    ByAssignee,
    Epics,
}

/// A task-list item parsed from an epic's body.
#[derive(Debug, Clone)]
struct TaskListItem {
    /// Whether this item is checked
    checked: bool,
    /// The text content of the task
    text: String,
    /// Issue reference if found (e.g. "#123" or "repo#456")
    issue_ref: Option<String>,
}

/// Fetched image: URL → raw bytes.
type ImageBytes = HashMap<String, Vec<u8>>;

/// State for the issue detail popup.
#[derive(Debug, Clone)]
enum CommentFetchState {
    NotStarted,
    Loading,
    Loaded(Vec<IssueComment>, ImageBytes),
    Error(String),
}

/// Persistent state for the issues page (stored as a thread-local to survive frame redraws).
struct IssuesPageState {
    active_tab: IssuesTab,
    selected_repo: Option<String>,
    selected_assignee: Option<String>,
    loaded_from_cache: bool,
    /// Currently open detail popup: (repo, number)
    detail_open: Option<(String, u64)>,
    /// Comment fetch state, shared with async task
    comments: Arc<Mutex<CommentFetchState>>,
    /// Set of expanded epics: (repo, number)
    expanded_epics: std::collections::HashSet<(String, u64)>,
}

impl Default for IssuesPageState {
    fn default() -> Self {
        Self {
            active_tab: IssuesTab::ByRepo,
            selected_repo: None,
            selected_assignee: None,
            loaded_from_cache: false,
            detail_open: None,
            comments: Arc::new(Mutex::new(CommentFetchState::NotStarted)),
            expanded_epics: std::collections::HashSet::new(),
        }
    }
}

impl IssuesTab {
    fn as_str(self) -> &'static str {
        match self {
            IssuesTab::ByRepo => "by_repo",
            IssuesTab::ByAssignee => "by_assignee",
            IssuesTab::Epics => "epics",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "by_assignee" => IssuesTab::ByAssignee,
            "epics" => IssuesTab::Epics,
            _ => IssuesTab::ByRepo,
        }
    }
}

thread_local! {
    static PAGE_STATE: std::cell::RefCell<IssuesPageState> = std::cell::RefCell::new(IssuesPageState::default());
    static MD_CACHE: std::cell::RefCell<CommonMarkCache> = std::cell::RefCell::new(CommonMarkCache::default());
}

fn with_state<F, R>(f: F) -> R
where
    F: FnOnce(&mut IssuesPageState) -> R,
{
    PAGE_STATE.with(|s| f(&mut s.borrow_mut()))
}

fn with_md_cache<F, R>(f: F) -> R
where
    F: FnOnce(&mut CommonMarkCache) -> R,
{
    MD_CACHE.with(|c| f(&mut c.borrow_mut()))
}

/// Convert an image URL to a `bytes://` URI for egui's DefaultBytesLoader.
fn image_bytes_uri(url: &str) -> String {
    format!("bytes://img/{:x}", {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        url.hash(&mut h);
        h.finish()
    })
}

/// Rewrite markdown image URLs to `bytes://` URIs so egui loads from our cache
/// instead of the ehttp loader (which can't auth with GitHub).
fn rewrite_image_urls(text: &str, images: &ImageBytes) -> String {
    if images.is_empty() {
        return text.to_string();
    }
    let mut result = text.to_string();
    for url in images.keys() {
        let bytes_uri = image_bytes_uri(url);
        result = result.replace(url, &bytes_uri);
    }
    result
}

/// Prepare GitHub markdown for rendering: sanitize HTML, compress bare URLs (outside code blocks).
fn prepare_markdown_with_images(text: &str, images: &ImageBytes) -> String {
    // 1. Sanitize HTML first (converts <img> to ![alt](src), strips tags)
    let sanitized = sanitize_html_in_markdown(text);

    // 2. Rewrite image URLs to bytes:// URIs
    let sanitized = rewrite_image_urls(&sanitized, images);

    // 3. Compress bare URLs, but skip fenced code blocks
    let mut result = String::with_capacity(sanitized.len());
    let mut in_code_block = false;

    for line in sanitized.split('\n') {
        if !result.is_empty() {
            result.push('\n');
        }
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            result.push_str(line);
        } else if in_code_block {
            // Inside code block — pass through verbatim
            result.push_str(line);
        } else {
            // Outside code block — compress bare URLs
            result.push_str(&compress_bare_urls(line));
        }
    }
    result
}

/// Parse task-list items from an epic's body markdown.
fn parse_task_list_items(body: Option<&str>) -> Vec<TaskListItem> {
    let body = match body {
        Some(b) => b,
        None => return Vec::new(),
    };

    let mut items = Vec::new();

    thread_local! {
        static ISSUE_REF_RE: Regex = Regex::new(r"#(\d+)|([a-zA-Z0-9\-_]+)#(\d+)").unwrap();
    }

    for line in body.lines() {
        let trimmed = line.trim();

        // Check for task-list items
        let (checked, text) = if trimmed.starts_with("- [x]") || trimmed.starts_with("- [X]") {
            (
                true,
                trimmed
                    .strip_prefix("- [x]")
                    .or_else(|| trimmed.strip_prefix("- [X]"))
                    .unwrap_or("")
                    .trim(),
            )
        } else if trimmed.starts_with("- [ ]") {
            (false, trimmed.strip_prefix("- [ ]").unwrap_or("").trim())
        } else {
            continue;
        };

        // Look for issue references in the task text
        let issue_ref = ISSUE_REF_RE.with(|re| re.find(text).map(|m| m.as_str().to_string()));

        items.push(TaskListItem {
            checked,
            text: text.to_string(),
            issue_ref,
        });
    }

    items
}

/// Convert HTML image tags to markdown syntax and strip other HTML tags.
/// GitHub issues commonly contain `<img src="..." />` and other HTML.
fn sanitize_html_in_markdown(text: &str) -> String {
    use regex::Regex;
    thread_local! {
        // Match <img> tags, capturing src and optional alt attributes (in either order)
        static IMG_RE: Regex = Regex::new(
            r#"(?i)<img\s[^>]*?src\s*=\s*["']([^"']+)["'][^>]*?/?>"#
        ).unwrap();
        static IMG_ALT_RE: Regex = Regex::new(
            r#"(?i)alt\s*=\s*["']([^"']*)["']"#
        ).unwrap();
        // Match <details>...</details> blocks (often contain verbose logs)
        static DETAILS_RE: Regex = Regex::new(
            r"(?is)<details[^>]*>.*?</details>"
        ).unwrap();
        // Match <br> / <br/> tags
        static BR_RE: Regex = Regex::new(r"(?i)<br\s*/?>").unwrap();
        // Block-level closing/opening tags that imply line breaks
        static BLOCK_RE: Regex = Regex::new(
            r"(?i)</?(p|div|tr|li|h[1-6]|blockquote|section|article|header|footer|nav|main|aside|figure|figcaption|dd|dt)\s*[^>]*>"
        ).unwrap();
        // <hr> → thematic break
        static HR_RE: Regex = Regex::new(r"(?i)<hr\s*/?>").unwrap();
        // Match any remaining HTML tags
        static TAG_RE: Regex = Regex::new(r"</?[a-zA-Z][^>]*>").unwrap();
    }

    // Convert <img src="..." alt="..."> to ![alt](src)
    let result = IMG_RE.with(|img_re| {
        IMG_ALT_RE.with(|alt_re| {
            img_re
                .replace_all(text, |caps: &regex::Captures| {
                    let src = &caps[1];
                    let full_tag = &caps[0];
                    let alt = alt_re
                        .captures(full_tag)
                        .and_then(|c| c.get(1))
                        .map(|m| m.as_str())
                        .unwrap_or("image");
                    format!("![{}]({})", alt, src)
                })
                .into_owned()
        })
    });

    // Collapse <details> blocks
    let result = DETAILS_RE.with(|re| {
        re.replace_all(&result, "*(collapsed details)*")
            .into_owned()
    });

    // Replace <br> with newlines
    let result = BR_RE.with(|re| re.replace_all(&result, "\n").into_owned());

    // Replace <hr> with markdown thematic break
    let result = HR_RE.with(|re| re.replace_all(&result, "\n\n---\n\n").into_owned());

    // Replace block-level tags with newlines
    let result = BLOCK_RE.with(|re| re.replace_all(&result, "\n").into_owned());

    // Strip remaining inline HTML tags (keep content between them)
    let result = TAG_RE.with(|re| re.replace_all(&result, "").into_owned());

    // Collapse runs of 3+ newlines down to 2 (paragraph break)
    let mut cleaned = String::with_capacity(result.len());
    let mut newline_count = 0;
    for ch in result.chars() {
        if ch == '\n' {
            newline_count += 1;
            if newline_count <= 2 {
                cleaned.push(ch);
            }
        } else {
            newline_count = 0;
            cleaned.push(ch);
        }
    }
    cleaned
}

/// Issues page entry point.
pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    _filters: &GlobalFilters,
    mode: ThemeMode,
    cache: &Option<Arc<Cache>>,
    config: &Option<crate::config::Config>,
    runtime: &tokio::runtime::Runtime,
) -> Option<View> {
    state.note_view_shown("issues");
    let all_issues = state.get_issues();
    let open_count = all_issues.iter().filter(|i| i.is_open()).count();

    let project_scope = state.has_project_scope();

    // Load saved selections from cache on first frame
    with_state(|s| {
        if !s.loaded_from_cache {
            s.loaded_from_cache = true;
            if let Some(ref c) = cache {
                if let Ok(Some(tab)) = c.get_meta("issues_tab") {
                    s.active_tab = IssuesTab::from_str(&tab);
                }
                if let Ok(Some(repo)) = c.get_meta("issues_selected_repo") {
                    s.selected_repo = Some(repo);
                }
                if let Ok(Some(assignee)) = c.get_meta("issues_selected_assignee") {
                    s.selected_assignee = Some(assignee);
                }
            }
        }
    });

    // ── Header ─────────────────────────────────────────────────────────────
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Issues")
                .size(26.0)
                .color(Colors::text_primary(mode))
                .strong(),
        );
        if open_count > 0 {
            theme::status_pill(ui, &format!("{} open", open_count), Colors::GREEN);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let refresh = ui.add(
                egui::Button::new(
                    egui::RichText::new(format!("{} Refresh", icons::ARROWS_ROTATE)).size(12.0),
                )
                .corner_radius(6.0),
            );
            if refresh.clicked() {
                state.request_refresh();
            }
        });
    });
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Issues across all tracked repositories")
                .color(Colors::text_secondary(mode)),
        );
        // Show Projects V2 scope status
        if project_scope == Some(true) {
            ui.add_space(12.0);
            theme::status_pill(
                ui,
                "Board connected",
                egui::Color32::from_rgb(100, 160, 100),
            );
        }
    });
    crate::ui::pull_requests::lazy_poll_caption(ui, state, mode, "polls while open");
    ui.add_space(8.0);

    // ── Re-auth banner when read:project scope is missing ──────────────────
    if project_scope == Some(false) {
        theme::card(ui, mode, |ui| {
            ui.horizontal(|ui| {
                ui.label(icons::icon(
                    icons::CIRCLE_INFO,
                    14.0,
                    Colors::ACCENT,
                ));
                ui.label(
                    egui::RichText::new(
                        "Board status requires the read:project scope. Re-authenticate in Settings to enable it.",
                    )
                    .size(12.0)
                    .color(Colors::text_secondary(mode)),
                );
            });
        });
        ui.add_space(4.0);
    }

    // ── Tab Bar ────────────────────────────────────────────────────────────
    let active_tab = with_state(|s| s.active_tab);
    ui.horizontal(|ui| {
        for (tab, label) in [
            (IssuesTab::ByRepo, "By Repo"),
            (IssuesTab::ByAssignee, "By Assignee"),
            (IssuesTab::Epics, "Epics"),
        ] {
            let is_active = active_tab == tab;
            let text = egui::RichText::new(label)
                .size(13.0)
                .strong()
                .color(if is_active {
                    Colors::ACCENT
                } else {
                    Colors::text_secondary(mode)
                });

            let resp = ui.add(egui::Label::new(text).sense(egui::Sense::click()));
            if resp.clicked() {
                with_state(|s| s.active_tab = tab);
                if let Some(ref c) = cache {
                    let _ = c.set_meta("issues_tab", tab.as_str());
                }
            }
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }

            // Underline for active tab
            if is_active {
                let rect = resp.rect;
                ui.painter().line_segment(
                    [rect.left_bottom(), rect.right_bottom()],
                    egui::Stroke::new(2.0_f32, Colors::ACCENT),
                );
            }
            ui.add_space(12.0);
        }
    });
    ui.add_space(8.0);
    ui.separator();
    ui.add_space(8.0);

    // ── Tab Content ────────────────────────────────────────────────────────
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| match active_tab {
            IssuesTab::ByRepo => show_by_repo(ui, state, mode, &all_issues, cache),
            IssuesTab::ByAssignee => show_by_assignee(ui, state, mode, &all_issues, cache),
            IssuesTab::Epics => show_epics(ui, mode, &all_issues),
        });

    // ── Issue Detail Popup ─────────────────────────────────────────────────
    show_detail_popup(ui, mode, state, &all_issues, config, runtime, cache);

    None
}

// ── By Repo Tab ────────────────────────────────────────────────────────────

fn show_by_repo(
    ui: &mut egui::Ui,
    _state: &AppState,
    mode: ThemeMode,
    all_issues: &[GitHubIssue],
    cache: &Option<Arc<Cache>>,
) {
    // Get list of repos that have issues
    let mut repo_names: Vec<String> = all_issues
        .iter()
        .filter(|i| i.is_open())
        .map(|i| i.repo.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    repo_names.sort();

    if repo_names.is_empty() {
        theme::card(ui, mode, |ui| {
            ui.label(egui::RichText::new("No open issues found").color(Colors::text_muted(mode)));
        });
        return;
    }

    // Ensure selected repo is valid
    let selected = with_state(|s| {
        if s.selected_repo.is_none() || !repo_names.contains(s.selected_repo.as_ref().unwrap()) {
            s.selected_repo = Some(repo_names[0].clone());
        }
        s.selected_repo.clone().unwrap()
    });

    // Repo selector
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Repository:")
                .size(12.0)
                .color(Colors::text_secondary(mode)),
        );
        egui::ComboBox::from_id_salt("issues_repo_selector")
            .selected_text(&selected)
            .show_ui(ui, |ui| {
                for name in &repo_names {
                    let count = all_issues
                        .iter()
                        .filter(|i| i.repo == *name && i.is_open())
                        .count();
                    let label = format!("{} ({})", name, count);
                    if ui
                        .selectable_value(&mut String::new(), String::new(), label)
                        .clicked()
                    {
                        with_state(|s| s.selected_repo = Some(name.clone()));
                        if let Some(ref c) = cache {
                            let _ = c.set_meta("issues_selected_repo", name);
                        }
                    }
                }
            });
    });
    ui.add_space(8.0);

    // Get issues for selected repo
    let repo_issues: Vec<&GitHubIssue> = all_issues
        .iter()
        .filter(|i| i.repo == selected && i.is_open())
        .collect();

    // Split into epics and regular issues
    let epics: Vec<&&GitHubIssue> = repo_issues.iter().filter(|i| i.is_epic).collect();
    let regular: Vec<&&GitHubIssue> = repo_issues.iter().filter(|i| !i.is_epic).collect();

    // Epics section
    if !epics.is_empty() {
        theme::section_header(ui, mode, &format!("Epics ({})", epics.len()));
        theme::card(ui, mode, |ui| {
            for epic in &epics {
                ui.horizontal(|ui| {
                    // Epic progress
                    if let Some((checked, total)) = epic.epic_progress {
                        let frac = if total > 0 {
                            checked as f32 / total as f32
                        } else {
                            0.0
                        };
                        let bar = egui::ProgressBar::new(frac)
                            .desired_width(60.0)
                            .text(format!("{}/{}", checked, total));
                        ui.add(bar);
                    }
                    // Project status pill
                    if let Some(ref status) = epic.project_status {
                        let color = project_status_color(status);
                        theme::status_pill(ui, status, color);
                    } else {
                        ui.label(
                            egui::RichText::new("—")
                                .size(11.0)
                                .color(Colors::text_muted(mode)),
                        );
                    }
                    // Title — clickable, opens detail
                    let title = epic
                        .title
                        .strip_prefix("EPIC : ")
                        .or_else(|| epic.title.strip_prefix("EPIC:"))
                        .unwrap_or(&epic.title)
                        .trim();
                    let resp = ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("#{} {}", epic.number, title))
                                .size(13.0)
                                .strong()
                                .color(Colors::ACCENT),
                        )
                        .sense(egui::Sense::click()),
                    );
                    if resp.clicked() {
                        open_detail(epic);
                    }
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    resp.on_hover_text(&epic.html_url);
                });
            }
        });
        ui.add_space(4.0);
    }

    // Regular issues
    if regular.is_empty() && epics.is_empty() {
        theme::card(ui, mode, |ui| {
            ui.label(
                egui::RichText::new("No open issues for this repository")
                    .color(Colors::text_muted(mode)),
            );
        });
    } else if !regular.is_empty() {
        theme::section_header(ui, mode, &format!("Issues ({})", regular.len()));
        theme::card(ui, mode, |ui| {
            issue_table(
                ui,
                mode,
                "repo_issues",
                &regular.into_iter().copied().collect::<Vec<_>>(),
            );
        });
    }

    // Related internal-docs issues
    let internal_docs_issues: Vec<&GitHubIssue> = all_issues
        .iter()
        .filter(|i| {
            i.repo == "internal-docs"
                && i.is_open()
                && i.matched_repos.iter().any(|r| r == &selected)
        })
        .collect();

    if !internal_docs_issues.is_empty() {
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(format!(
                "Related from internal-docs ({})",
                internal_docs_issues.len()
            ))
            .size(11.0)
            .color(Colors::text_muted(mode)),
        );
        theme::card(ui, mode, |ui| {
            issue_table(ui, mode, "internal_docs_issues", &internal_docs_issues);
        });
    }
}

// ── By Assignee Tab ────────────────────────────────────────────────────────

fn show_by_assignee(
    ui: &mut egui::Ui,
    _state: &AppState,
    mode: ThemeMode,
    all_issues: &[GitHubIssue],
    cache: &Option<Arc<Cache>>,
) {
    // Group open, non-epic issues by assignee
    let mut by_assignee: HashMap<String, Vec<&GitHubIssue>> = HashMap::new();
    for issue in all_issues.iter().filter(|i| i.is_open() && !i.is_epic) {
        if issue.assignees.is_empty() {
            by_assignee.entry(String::new()).or_default().push(issue);
        } else {
            for assignee in &issue.assignees {
                by_assignee.entry(assignee.clone()).or_default().push(issue);
            }
        }
    }

    if by_assignee.is_empty() {
        theme::card(ui, mode, |ui| {
            ui.label(egui::RichText::new("No open issues found").color(Colors::text_muted(mode)));
        });
        return;
    }

    // Sort: named assignees first (alphabetically), then unassigned last
    let mut assignees: Vec<String> = by_assignee.keys().cloned().collect();
    assignees.sort_by(|a, b| {
        if a.is_empty() {
            std::cmp::Ordering::Greater
        } else if b.is_empty() {
            std::cmp::Ordering::Less
        } else {
            a.to_lowercase().cmp(&b.to_lowercase())
        }
    });

    // Ensure selected assignee is valid
    let selected = with_state(|s| {
        if s.selected_assignee.is_none()
            || !assignees.contains(s.selected_assignee.as_ref().unwrap())
        {
            s.selected_assignee = Some(assignees[0].clone());
        }
        s.selected_assignee.clone().unwrap()
    });

    // Assignee selector
    let display_selected = if selected.is_empty() {
        "Unassigned".to_string()
    } else {
        selected.clone()
    };
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Assignee:")
                .size(12.0)
                .color(Colors::text_secondary(mode)),
        );
        egui::ComboBox::from_id_salt("issues_assignee_selector")
            .selected_text(&display_selected)
            .show_ui(ui, |ui| {
                for name in &assignees {
                    let count = by_assignee.get(name).map_or(0, |v| v.len());
                    let display = if name.is_empty() {
                        format!("Unassigned ({})", count)
                    } else {
                        format!("{} ({})", name, count)
                    };
                    if ui
                        .selectable_value(&mut String::new(), String::new(), display)
                        .clicked()
                    {
                        with_state(|s| s.selected_assignee = Some(name.clone()));
                        if let Some(ref c) = cache {
                            let _ = c.set_meta("issues_selected_assignee", name);
                        }
                    }
                }
            });
        // Show issue count for selected
        if let Some(issues) = by_assignee.get(&selected) {
            theme::status_pill(ui, &format!("{} issues", issues.len()), Colors::ACCENT);
        }
    });
    ui.add_space(8.0);

    // Show issues for selected assignee
    if let Some(issues) = by_assignee.get(&selected) {
        theme::card(ui, mode, |ui| {
            assignee_issue_table(ui, mode, &format!("assignee_{}", selected), issues);
        });
    }
}

// ── Epics Tab ──────────────────────────────────────────────────────────────

fn show_epics(ui: &mut egui::Ui, mode: ThemeMode, all_issues: &[GitHubIssue]) {
    let epics: Vec<&GitHubIssue> = all_issues
        .iter()
        .filter(|i| i.is_epic && i.is_open())
        .collect();

    if epics.is_empty() {
        theme::card(ui, mode, |ui| {
            ui.label(egui::RichText::new("No open epics found").color(Colors::text_muted(mode)));
        });
        return;
    }

    // product epics first, then others sorted by repo
    let mut product_epics: Vec<&&GitHubIssue> =
        epics.iter().filter(|i| i.repo == "product").collect();
    let mut other_epics: Vec<&&GitHubIssue> =
        epics.iter().filter(|i| i.repo != "product").collect();
    product_epics.sort_by_key(|e| e.number);
    other_epics.sort_by(|a, b| a.repo.cmp(&b.repo).then(a.number.cmp(&b.number)));

    if !product_epics.is_empty() {
        theme::section_header(
            ui,
            mode,
            &format!("Product Epics ({})", product_epics.len()),
        );
        theme::card(ui, mode, |ui| {
            epic_table(ui, mode, "product_epics", &product_epics);
        });
        ui.add_space(4.0);
    }

    if !other_epics.is_empty() {
        theme::section_header(ui, mode, &format!("Repo Epics ({})", other_epics.len()));
        theme::card(ui, mode, |ui| {
            epic_table(ui, mode, "other_epics", &other_epics);
        });
    }
}

// ── Tables ─────────────────────────────────────────────────────────────────

/// Standard issue table: #, TITLE, LABELS, STATUS, ASSIGNEE, AGE, link
fn issue_table(ui: &mut egui::Ui, mode: ThemeMode, table_id: &str, issues: &[&GitHubIssue]) {
    egui::Grid::new(egui::Id::new(table_id))
        .num_columns(7)
        .spacing([16.0, 6.0])
        .min_col_width(30.0)
        .striped(true)
        .show(ui, |ui| {
            for col in &["#", "TITLE", "LABELS", "STATUS", "ASSIGNEE", "AGE", ""] {
                ui.label(
                    egui::RichText::new(*col)
                        .size(10.0)
                        .strong()
                        .color(Colors::text_muted(mode)),
                );
            }
            ui.end_row();

            for issue in issues {
                issue_row(ui, mode, issue, true);
            }
        });
}

/// Compact assignee table: REPO, #, TITLE, LABELS, STATUS, AGE
fn assignee_issue_table(
    ui: &mut egui::Ui,
    mode: ThemeMode,
    table_id: &str,
    issues: &[&GitHubIssue],
) {
    egui::Grid::new(egui::Id::new(table_id))
        .num_columns(6)
        .spacing([16.0, 6.0])
        .min_col_width(30.0)
        .striped(true)
        .show(ui, |ui| {
            for col in &["REPO", "#", "TITLE", "LABELS", "STATUS", "AGE"] {
                ui.label(
                    egui::RichText::new(*col)
                        .size(10.0)
                        .strong()
                        .color(Colors::text_muted(mode)),
                );
            }
            ui.end_row();

            for issue in issues {
                // Repo — clickable, opens detail
                let repo_resp = ui.add(
                    egui::Label::new(
                        egui::RichText::new(&issue.repo)
                            .monospace()
                            .size(12.0)
                            .color(Colors::ACCENT),
                    )
                    .sense(egui::Sense::click()),
                );
                if repo_resp.clicked() {
                    open_detail(issue);
                }
                if repo_resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                // #number — clickable, opens detail
                let resp = ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("#{}", issue.number))
                            .monospace()
                            .size(12.0)
                            .color(Colors::ACCENT),
                    )
                    .sense(egui::Sense::click()),
                );
                if resp.clicked() {
                    open_detail(issue);
                }
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                resp.on_hover_text(&issue.html_url);
                // Title — clickable, opens detail
                let title_short = truncate_title(&issue.title, 50);
                let title_resp = ui.add(
                    egui::Label::new(
                        egui::RichText::new(title_short)
                            .size(12.0)
                            .color(Colors::text_primary(mode)),
                    )
                    .sense(egui::Sense::click()),
                );
                if title_resp.clicked() {
                    open_detail(issue);
                }
                if title_resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                // Labels
                label_pills(ui, mode, &issue.labels);
                // Project status
                status_cell(ui, mode, issue);
                // Age
                ui.label(
                    egui::RichText::new(widgets::time_ago(&issue.created_at))
                        .size(11.0)
                        .color(Colors::text_muted(mode)),
                );
                ui.end_row();
            }
        });
}

/// Epic table with expand/collapse functionality: [+/-], REPO, #, EPIC NAME, PROGRESS, STATUS, AGE
fn epic_table(ui: &mut egui::Ui, mode: ThemeMode, _table_id: &str, epics: &[&&GitHubIssue]) {
    for epic in epics {
        let epic_key = (epic.repo.clone(), epic.number);
        let is_expanded = with_state(|state| state.expanded_epics.contains(&epic_key));

        // Main epic row
        ui.horizontal(|ui| {
            // Expand/collapse button
            let expand_text = if is_expanded { "▼" } else { "▶" };
            let expand_resp = ui.add(
                egui::Button::new(egui::RichText::new(expand_text).size(10.0))
                    .small()
                    .frame(false),
            );
            if expand_resp.clicked() {
                with_state(|state| {
                    if state.expanded_epics.contains(&epic_key) {
                        state.expanded_epics.remove(&epic_key);
                    } else {
                        state.expanded_epics.insert(epic_key.clone());
                    }
                });
            }

            // Repo — clickable, opens detail
            let repo_resp = ui.add(
                egui::Label::new(
                    egui::RichText::new(&epic.repo)
                        .monospace()
                        .size(12.0)
                        .color(Colors::ACCENT),
                )
                .sense(egui::Sense::click()),
            );
            if repo_resp.clicked() {
                open_detail(epic);
            }
            if repo_resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }

            // #number — clickable, opens detail
            let resp = ui.add(
                egui::Label::new(
                    egui::RichText::new(format!("#{}", epic.number))
                        .monospace()
                        .size(12.0)
                        .color(Colors::ACCENT),
                )
                .sense(egui::Sense::click()),
            );
            if resp.clicked() {
                open_detail(epic);
            }
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            resp.on_hover_text(&epic.html_url);

            // Epic name (strip "EPIC :" prefix)
            let title = epic
                .title
                .strip_prefix("EPIC : ")
                .or_else(|| epic.title.strip_prefix("EPIC:"))
                .unwrap_or(&epic.title)
                .trim();
            let title_short = truncate_title(title, 40);
            ui.label(
                egui::RichText::new(title_short)
                    .size(12.0)
                    .strong()
                    .color(Colors::text_primary(mode)),
            );

            // Progress bar
            if let Some((checked, total)) = epic.epic_progress {
                let frac = if total > 0 {
                    checked as f32 / total as f32
                } else {
                    0.0
                };
                let bar = egui::ProgressBar::new(frac)
                    .desired_width(80.0)
                    .text(format!("{}/{}", checked, total));
                ui.add(bar);
            } else {
                ui.label(
                    egui::RichText::new("—")
                        .size(11.0)
                        .color(Colors::text_muted(mode)),
                );
            }

            // Project status
            if let Some(ref status) = epic.project_status {
                let color = project_status_color(status);
                theme::status_pill(ui, status, color);
            } else {
                ui.label(egui::RichText::new("Open").size(10.0).color(Colors::GREEN));
            }

            // Age
            ui.label(
                egui::RichText::new(widgets::time_ago(&epic.updated_at))
                    .size(11.0)
                    .color(Colors::text_muted(mode)),
            );
        });

        // Show task-list items if expanded
        if is_expanded {
            let task_items = parse_task_list_items(epic.body.as_deref());
            if !task_items.is_empty() {
                ui.indent("epic_tasks", |ui| {
                    ui.add_space(4.0);
                    for item in task_items.iter() {
                        ui.horizontal(|ui| {
                            let checkbox_text = if item.checked { "☑" } else { "☐" };
                            let checkbox_color = if item.checked {
                                Colors::GREEN
                            } else {
                                Colors::text_muted(mode)
                            };
                            ui.label(
                                egui::RichText::new(checkbox_text)
                                    .size(12.0)
                                    .color(checkbox_color),
                            );

                            // Task text
                            let task_color = if item.checked {
                                Colors::text_muted(mode)
                            } else {
                                Colors::text_primary(mode)
                            };
                            ui.label(egui::RichText::new(&item.text).size(11.0).color(task_color));

                            // Issue reference if present
                            if let Some(ref issue_ref) = item.issue_ref {
                                ui.label(
                                    egui::RichText::new(issue_ref)
                                        .size(10.0)
                                        .monospace()
                                        .color(Colors::ACCENT),
                                );
                            }
                        });
                    }
                    ui.add_space(4.0);
                });
            } else {
                ui.indent("epic_no_tasks", |ui| {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("No task-list items found")
                            .size(10.0)
                            .color(Colors::text_muted(mode)),
                    );
                    ui.add_space(4.0);
                });
            }
        }

        ui.add_space(8.0);
    }
}

/// Render a single issue row for the standard issue table.
fn issue_row(ui: &mut egui::Ui, mode: ThemeMode, issue: &GitHubIssue, show_assignee: bool) {
    let is_closed = !issue.is_open();
    let text_color = if is_closed {
        Colors::text_muted(mode)
    } else {
        Colors::ACCENT
    };

    // #number — clickable, opens detail
    let resp = ui.add(
        egui::Label::new(
            egui::RichText::new(format!("#{}", issue.number))
                .monospace()
                .size(12.0)
                .color(text_color),
        )
        .sense(egui::Sense::click()),
    );
    if resp.clicked() {
        open_detail(issue);
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text(&issue.html_url);

    // Title — clickable, opens detail
    let title_short = truncate_title(&issue.title, 55);
    let title_color = if is_closed {
        Colors::text_muted(mode)
    } else {
        Colors::text_primary(mode)
    };
    let title_resp = ui.add(
        egui::Label::new(
            egui::RichText::new(title_short)
                .size(12.0)
                .color(title_color),
        )
        .sense(egui::Sense::click()),
    );
    if title_resp.clicked() {
        open_detail(issue);
    }
    if title_resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    title_resp.on_hover_text(&issue.html_url);

    // Labels
    label_pills(ui, mode, &issue.labels);

    // Project status (always shown)
    status_cell(ui, mode, issue);

    // Assignee
    if show_assignee {
        let assignee_text = if issue.assignees.is_empty() {
            "—".to_string()
        } else {
            issue.assignees.join(", ")
        };
        ui.label(
            egui::RichText::new(assignee_text)
                .size(11.0)
                .color(Colors::text_secondary(mode)),
        );
    }

    // Age
    ui.label(
        egui::RichText::new(widgets::time_ago(&issue.created_at))
            .size(11.0)
            .color(Colors::text_muted(mode)),
    );

    // External link icon — opens GitHub directly
    let link_resp = ui.add(
        egui::Label::new(icons::icon(
            icons::ARROW_UP_RIGHT_FROM_SQUARE,
            10.0,
            Colors::text_muted(mode),
        ))
        .sense(egui::Sense::click()),
    );
    if link_resp.clicked() {
        let _ = open::that(&issue.html_url);
    }
    if link_resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    ui.end_row();
}

/// Render a status cell — always shown.
fn status_cell(ui: &mut egui::Ui, _mode: ThemeMode, issue: &GitHubIssue) {
    if let Some(ref status) = issue.project_status {
        let color = project_status_color(status);
        theme::status_pill(ui, status, color);
    } else {
        // Fallback: show the issue's own open/closed state
        let (label, color) = if issue.is_open() {
            ("Open", Colors::GREEN)
        } else {
            ("Closed", egui::Color32::from_rgb(150, 80, 150))
        };
        theme::status_pill(ui, label, color);
    }
}

/// Open the detail popup for an issue.
fn open_detail(issue: &GitHubIssue) {
    with_state(|s| {
        s.detail_open = Some((issue.repo.clone(), issue.number));
        *s.comments.lock().unwrap() = CommentFetchState::NotStarted;
    });
}

// ── Detail Popup ───────────────────────────────────────────────────────────

fn show_detail_popup(
    ui: &mut egui::Ui,
    mode: ThemeMode,
    state: &AppState,
    all_issues: &[GitHubIssue],
    config: &Option<crate::config::Config>,
    runtime: &tokio::runtime::Runtime,
    cache: &Option<Arc<Cache>>,
) {
    let detail = with_state(|s| s.detail_open.clone());
    let Some((repo, number)) = detail else {
        return;
    };

    let issue = all_issues
        .iter()
        .find(|i| i.repo == repo && i.number == number);
    let Some(issue) = issue else {
        with_state(|s| s.detail_open = None);
        return;
    };

    // Kick off comment fetch if not started
    let fetch_state = with_state(|s| {
        let state = s.comments.lock().unwrap().clone();
        state
    });
    if matches!(fetch_state, CommentFetchState::NotStarted) {
        // Try loading cached comments + images instantly
        let cached_comments = cache
            .as_ref()
            .and_then(|c| c.load_comments(&repo, number).ok())
            .unwrap_or_default();
        let cached_images = cache
            .as_ref()
            .and_then(|c| c.load_all_images().ok())
            .unwrap_or_default();

        // Register cached images with egui immediately
        for (url, bytes) in &cached_images {
            let bytes_uri = image_bytes_uri(url);
            ui.ctx().include_bytes(bytes_uri, bytes.clone());
        }

        if !cached_comments.is_empty() {
            // Show cached data immediately, then refresh in background
            with_state(|s| {
                *s.comments.lock().unwrap() =
                    CommentFetchState::Loaded(cached_comments, cached_images);
            });
        } else {
            with_state(|s| {
                *s.comments.lock().unwrap() = CommentFetchState::Loading;
            });
        }

        // Always fetch fresh data in background
        let comments_handle = with_state(|s| s.comments.clone());
        let org = config
            .as_ref()
            .map(|c| c.github.org.clone())
            .unwrap_or_default();
        let token = config
            .as_ref()
            .and_then(|c| c.resolve_github_token().ok().flatten());
        let repo_name = repo.clone();
        let issue_number = number;
        let issue_body = issue.body.clone().unwrap_or_default();
        let ctx = ui.ctx().clone();
        let cache_arc = cache.clone();
        let gh_state = state.clone();
        runtime.spawn(async move {
            let result = match token.clone() {
                Some(t) => match GhClient::new(t, cache_arc.clone(), gh_state) {
                    Ok(client) => fetch_comments(&client, &org, &repo_name, issue_number).await,
                    Err(e) => Err(e),
                },
                None => Err(anyhow::anyhow!("No GitHub token available")),
            };
            match result {
                Ok(comments) => {
                    // Collect all image URLs from issue body + comments
                    let mut image_urls = extract_image_urls(&issue_body);
                    for c in &comments {
                        image_urls.extend(extract_image_urls(&c.body));
                    }
                    // Fetch images with auth
                    let images = fetch_images(&image_urls, token.as_deref()).await;
                    // Register fetched images with egui using bytes:// URIs
                    for (url, bytes) in &images {
                        let bytes_uri = image_bytes_uri(url);
                        ctx.include_bytes(bytes_uri, bytes.clone());
                    }
                    // Save to cache
                    if let Some(ref cache) = cache_arc {
                        let _ = cache.save_comments(&repo_name, issue_number, &comments);
                        for (url, bytes) in &images {
                            let _ = cache.save_image(url, bytes);
                        }
                    }
                    *comments_handle.lock().unwrap() = CommentFetchState::Loaded(comments, images);
                }
                Err(e) => {
                    // Only set error if we don't already have cached data
                    let mut state = comments_handle.lock().unwrap();
                    if !matches!(&*state, CommentFetchState::Loaded(..)) {
                        *state = CommentFetchState::Error(format!("{}", e));
                    }
                }
            }
            ctx.request_repaint();
        });
    }

    // Render the popup as an egui Window
    let mut open = true;
    let title_text = format!("{} #{}", issue.repo, issue.number);

    egui::Window::new(&title_text)
        .id(egui::Id::new("issue_detail_popup"))
        .open(&mut open)
        .resizable(true)
        .default_width(700.0)
        .max_width(800.0)
        .default_height(500.0)
        .scroll([true, true])
        .show(ui.ctx(), |ui| {
            ui.set_max_width(780.0);
            // ── Header ─────────────────────────────────────────────────
            ui.horizontal(|ui| {
                // State pill
                let (state_label, state_color) = if issue.is_open() {
                    ("Open", Colors::GREEN)
                } else {
                    ("Closed", egui::Color32::from_rgb(150, 80, 150))
                };
                theme::status_pill(ui, state_label, state_color);

                // Project status
                if let Some(ref status) = issue.project_status {
                    let color = project_status_color(status);
                    theme::status_pill(ui, status, color);
                }
                if let Some(ref priority) = issue.project_priority {
                    theme::status_pill(ui, priority, egui::Color32::from_rgb(130, 130, 180));
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Refresh button
                    let refresh_resp = ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("{} Refresh", icons::ARROWS_ROTATE))
                                .size(12.0)
                                .color(Colors::text_secondary(mode)),
                        )
                        .sense(egui::Sense::click()),
                    );
                    if refresh_resp.clicked() {
                        with_state(|s| {
                            *s.comments.lock().unwrap() = CommentFetchState::NotStarted;
                        });
                    }
                    if refresh_resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }

                    ui.add_space(12.0);

                    // Open in GitHub button
                    let gh_resp = ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!(
                                "{} Open in GitHub",
                                icons::ARROW_UP_RIGHT_FROM_SQUARE
                            ))
                            .size(12.0)
                            .color(Colors::ACCENT),
                        )
                        .sense(egui::Sense::click()),
                    );
                    if gh_resp.clicked() {
                        let _ = open::that(&issue.html_url);
                    }
                    if gh_resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                });
            });

            ui.add_space(8.0);

            // Title
            ui.label(
                egui::RichText::new(&issue.title)
                    .size(18.0)
                    .strong()
                    .color(Colors::text_primary(mode)),
            );
            ui.add_space(8.0);

            // ── Metadata ───────────────────────────────────────────────
            egui::Grid::new("issue_detail_meta")
                .num_columns(2)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    meta_row(ui, mode, "Author", issue.author.as_deref().unwrap_or("—"));
                    let assignee_str = if issue.assignees.is_empty() {
                        "None".to_string()
                    } else {
                        issue.assignees.join(", ")
                    };
                    meta_row(ui, mode, "Assignees", &assignee_str);
                    meta_row(ui, mode, "Created", &widgets::time_ago(&issue.created_at));
                    meta_row(ui, mode, "Updated", &widgets::time_ago(&issue.updated_at));
                    if let Some(ref ms) = issue.milestone {
                        meta_row(ui, mode, "Milestone", ms);
                    }
                });
            // Labels
            if !issue.labels.is_empty() {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Labels")
                            .size(11.0)
                            .strong()
                            .color(Colors::text_muted(mode)),
                    );
                    label_pills(ui, mode, &issue.labels);
                });
            }

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(8.0);

            // Get fetched images for URL rewriting
            let fetched_images = with_state(|s| match &*s.comments.lock().unwrap() {
                CommentFetchState::Loaded(_, images) => images.clone(),
                _ => HashMap::new(),
            });

            // ── Body ───────────────────────────────────────────────────
            if let Some(ref body) = issue.body {
                if !body.trim().is_empty() {
                    ui.label(
                        egui::RichText::new("Description")
                            .size(13.0)
                            .strong()
                            .color(Colors::text_primary(mode)),
                    );
                    ui.add_space(4.0);
                    let processed = prepare_markdown_with_images(body, &fetched_images);
                    with_md_cache(|md_cache| {
                        CommonMarkViewer::new()
                            .max_image_width(Some(600))
                            .show(ui, md_cache, &processed);
                    });
                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(8.0);
                }
            }

            // ── Comments ───────────────────────────────────────────────
            let comment_state = with_state(|s| s.comments.lock().unwrap().clone());
            match comment_state {
                CommentFetchState::NotStarted | CommentFetchState::Loading => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            egui::RichText::new("Loading comments...")
                                .color(Colors::text_muted(mode)),
                        );
                    });
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(200));
                }
                CommentFetchState::Loaded(ref comments, ref images) => {
                    // Register images with egui using bytes:// URIs
                    for (url, bytes) in images {
                        let bytes_uri = image_bytes_uri(url);
                        ui.ctx().include_bytes(bytes_uri, bytes.clone());
                    }

                    ui.label(
                        egui::RichText::new(format!("Comments ({})", comments.len()))
                            .size(13.0)
                            .strong()
                            .color(Colors::text_primary(mode)),
                    );
                    ui.add_space(4.0);

                    if comments.is_empty() {
                        ui.label(
                            egui::RichText::new("No comments").color(Colors::text_muted(mode)),
                        );
                    } else {
                        for (ci, comment) in comments.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(
                                        comment.author.as_deref().unwrap_or("unknown"),
                                    )
                                    .size(12.0)
                                    .strong()
                                    .color(Colors::ACCENT),
                                );
                                ui.label(
                                    egui::RichText::new(widgets::time_ago(&comment.created_at))
                                        .size(11.0)
                                        .color(Colors::text_muted(mode)),
                                );
                            });
                            let processed =
                                prepare_markdown_with_images(&comment.body, &fetched_images);
                            ui.push_id(format!("comment_{}", ci), |ui| {
                                with_md_cache(|md_cache| {
                                    CommonMarkViewer::new()
                                        .max_image_width(Some(600))
                                        .show(ui, md_cache, &processed);
                                });
                            });
                            ui.add_space(8.0);
                            ui.separator();
                            ui.add_space(4.0);
                        }
                    }
                }
                CommentFetchState::Error(ref err) => {
                    ui.label(
                        egui::RichText::new(format!("Failed to load comments: {}", err))
                            .color(egui::Color32::from_rgb(220, 120, 50)),
                    );
                }
            }
        });

    if !open {
        with_state(|s| s.detail_open = None);
    }
}

fn meta_row(ui: &mut egui::Ui, mode: ThemeMode, label: &str, value: &str) {
    ui.label(
        egui::RichText::new(label)
            .size(11.0)
            .strong()
            .color(Colors::text_muted(mode)),
    );
    ui.label(
        egui::RichText::new(value)
            .size(12.0)
            .color(Colors::text_secondary(mode)),
    );
    ui.end_row();
}

/// Fetch comments for an issue through the conditional GitHub client, so a
/// reopened issue costs nothing while its comments are unchanged.
async fn fetch_comments(
    client: &GhClient,
    org: &str,
    repo: &str,
    number: u64,
) -> anyhow::Result<Vec<IssueComment>> {
    let route = format!(
        "/repos/{}/{}/issues/{}/comments?per_page=100",
        org, repo, number
    );
    let items = client
        .get(&route)
        .await?
        .as_array()
        .cloned()
        .unwrap_or_default();

    let mut comments = Vec::new();
    for item in items {
        let id = item.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let author = item
            .get("user")
            .and_then(|u| u.get("login"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let body = item
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let created_at = item
            .get("created_at")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now);
        let updated_at = item
            .get("updated_at")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now);
        let html_url = item
            .get("html_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        comments.push(IssueComment {
            id,
            author,
            body,
            created_at,
            updated_at,
            html_url,
        });
    }

    Ok(comments)
}

/// Extract markdown image URLs from text: `![alt](url)` and `<img src="url">`.
fn extract_image_urls(text: &str) -> Vec<String> {
    use regex::Regex;
    thread_local! {
        static MD_IMG_RE: Regex = Regex::new(r#"!\[[^\]]*\]\(([^)]+)\)"#).unwrap();
        static HTML_IMG_RE: Regex = Regex::new(
            r#"(?i)<img\s[^>]*?src\s*=\s*["']([^"']+)["'][^>]*?/?>"#
        ).unwrap();
    }

    let mut urls = Vec::new();
    MD_IMG_RE.with(|re| {
        for cap in re.captures_iter(text) {
            urls.push(cap[1].to_string());
        }
    });
    HTML_IMG_RE.with(|re| {
        for cap in re.captures_iter(text) {
            urls.push(cap[1].to_string());
        }
    });
    urls
}

/// Fetch image bytes from URLs, using auth for GitHub URLs.
async fn fetch_images(urls: &[String], token: Option<&str>) -> ImageBytes {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let mut images = HashMap::new();

    for url in urls {
        let mut req = client.get(url).header("User-Agent", "caravan");

        // Add auth for GitHub URLs (private repo images need it)
        if url.contains("github.com") || url.contains("githubusercontent.com") {
            if let Some(token) = token {
                req = req.header("Authorization", format!("Bearer {}", token));
            }
        }

        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(bytes) = resp.bytes().await {
                    images.insert(url.clone(), bytes.to_vec());
                }
            }
            Ok(resp) => {
                tracing::debug!(url = %url, status = %resp.status(), "Failed to fetch image");
            }
            Err(e) => {
                tracing::debug!(url = %url, err = %e, "Failed to fetch image");
            }
        }
    }

    images
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Render label pills in a horizontal flow.
fn label_pills(ui: &mut egui::Ui, _mode: ThemeMode, labels: &[crate::models::issue::IssueLabel]) {
    if labels.is_empty() {
        ui.label("");
        return;
    }
    ui.horizontal(|ui| {
        for label in labels.iter().take(3) {
            let bg = parse_hex_color(&label.color).unwrap_or(egui::Color32::from_gray(80));
            // Use white or dark text depending on background brightness
            let fg = if luminance(bg) > 0.5 {
                egui::Color32::from_gray(30)
            } else {
                egui::Color32::WHITE
            };
            let text = egui::RichText::new(&label.name).size(10.0).color(fg);
            let label_resp = ui.add(egui::Label::new(text).selectable(false));
            // Draw background behind the label
            let rect = label_resp.rect.expand2(egui::vec2(3.0, 1.0));
            ui.painter().rect_filled(rect, 3.0, bg);
            // Redraw text on top of background
            ui.painter().text(
                label_resp.rect.center(),
                egui::Align2::CENTER_CENTER,
                &label.name,
                egui::FontId::proportional(10.0),
                fg,
            );
        }
        if labels.len() > 3 {
            ui.label(
                egui::RichText::new(format!("+{}", labels.len() - 3))
                    .size(10.0)
                    .color(egui::Color32::from_gray(120)),
            );
        }
    });
}

fn truncate_title(title: &str, max: usize) -> String {
    if title.len() > max {
        format!("{}…", &title[..max - 1])
    } else {
        title.to_string()
    }
}

fn parse_hex_color(hex: &str) -> Option<egui::Color32> {
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(egui::Color32::from_rgb(r, g, b))
}

fn luminance(c: egui::Color32) -> f32 {
    // Simple perceived luminance
    (0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32) / 255.0
}

/// Map common GitHub Projects V2 status names to colors.
fn project_status_color(status: &str) -> egui::Color32 {
    match status.to_lowercase().as_str() {
        "done" | "completed" | "closed" => Colors::GREEN,
        "in progress" | "in review" | "active" => Colors::ACCENT,
        "todo" | "backlog" | "ready" => egui::Color32::from_rgb(150, 150, 150),
        "blocked" | "on hold" => egui::Color32::from_rgb(220, 120, 50),
        _ => egui::Color32::from_rgb(130, 130, 130),
    }
}
