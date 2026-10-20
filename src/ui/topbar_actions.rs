//! Top-bar actions: per-environment Flux sync buttons and the
//! promote button (source to target from config) (with its modal).

use std::time::Instant;

use eframe::egui;

use crate::config::Config;
use crate::gitops_mirror::Mirror;
use crate::state::{AppState, FluxSyncState, KustomizationInfo};
use crate::ui::icons;
use crate::ui::theme::{env_label, Colors, ThemeMode};
use crate::ui::toast::Toaster;

/// Temp-data key: when the applied-distance task last ran.
const DISTANCE_TASK_ID: &str = "applied_distance_task";
/// Temp-data key: cached (checked at, present) for the mirror on disk.
const MIRROR_PRESENT_ID: &str = "promote_mirror_present";

const DONE_VISIBLE_SECS: i64 = 600;
const DISTANCE_INTERVAL_SECS: u64 = 60;

pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    config: &Option<Config>,
    mode: ThemeMode,
    runtime: &tokio::runtime::Runtime,
    toaster: &mut Toaster,
) {
    let Some(config) = config else {
        return;
    };
    maybe_spawn_distance_task(ui.ctx(), state, config, runtime);

    let syncs = state.get_flux_syncs();
    let head_sha = state.get_gitops_head().map(|h| h.sha);
    ui.spacing_mut().item_spacing.x = 6.0_f32;
    for env_config in config.environments.with_clusters() {
        let env = &env_config.id.as_str();
        let kustomization = state.get_environment(env).and_then(|e| e.kustomization);
        let behind = state.commits_behind_main(env);
        let clicked = sync_button(
            ui,
            mode,
            env,
            syncs.get(*env),
            kustomization.as_ref(),
            behind,
            head_sha.as_deref(),
        );
        if clicked {
            crate::actions::flux::spawn_sync(
                runtime,
                env.to_string(),
                env_config.clone(),
                state.clone(),
            );
        }
    }

    ui.add_space(6.0_f32);
    promote_button(ui, state, config, mode, runtime);
    crate::ui::promote::show(ui.ctx(), state, config, mode, runtime, toaster);
}

fn sync_button(
    ui: &mut egui::Ui,
    mode: ThemeMode,
    env: &str,
    sync: Option<&FluxSyncState>,
    kustomization: Option<&KustomizationInfo>,
    behind: Option<u64>,
    head_sha: Option<&str>,
) -> bool {
    let label = env_label(env);
    let now = chrono::Utc::now();
    let applied_sha = kustomization.and_then(|k| k.applied_sha.as_deref());
    let needs_sync = match behind {
        Some(n) => n > 0,
        None => matches!((applied_sha, head_sha), (Some(a), Some(h)) if a != h),
    };

    let (job, enabled) = match sync {
        Some(FluxSyncState::Running { phase, started }) => {
            let elapsed = (now - *started).num_seconds().max(0);
            (
                button_text(
                    None,
                    &format!("{label} · {phase} · {elapsed}s"),
                    Colors::BLUE,
                ),
                false,
            )
        }
        Some(FluxSyncState::Done { revision, finished })
            if (now - *finished).num_seconds() < DONE_VISIBLE_SECS =>
        {
            (
                button_text(
                    Some((icons::CIRCLE_CHECK, Colors::GREEN)),
                    &format!(
                        "{label} {} · {}",
                        short_revision(revision),
                        ago(now - *finished)
                    ),
                    Colors::GREEN,
                ),
                true,
            )
        }
        Some(FluxSyncState::Failed { error, .. }) => (
            button_text(
                Some((icons::CIRCLE_XMARK, Colors::RED)),
                &format!("{label} {} · retry", short_error(error)),
                Colors::RED,
            ),
            true,
        ),
        _ => {
            let dot = if needs_sync {
                Colors::ORANGE
            } else {
                Colors::GREEN
            };
            (
                button_text(
                    Some((icons::CIRCLE, dot)),
                    &format!("Sync {label}"),
                    Colors::text_primary(mode),
                ),
                true,
            )
        }
    };

    let mut hover = String::new();
    match kustomization.and_then(|k| k.last_applied_revision.as_deref()) {
        Some(rev) => hover.push_str(&format!("applied: {rev}")),
        None => hover.push_str("applied: unknown"),
    }
    hover.push('\n');
    match behind {
        Some(0) => hover.push_str("main is applied"),
        Some(n) => hover.push_str(&format!("main is ahead by {n} commit{}", plural(n))),
        None if needs_sync => hover.push_str("main is ahead"),
        None => hover.push_str("main distance not computed yet"),
    }
    match sync {
        Some(FluxSyncState::Done { revision, finished }) => {
            hover.push_str(&format!(
                "\nlast sync: ok, {revision}, {}",
                finished.with_timezone(&chrono::Local).format("%H:%M:%S %Z")
            ));
        }
        Some(FluxSyncState::Failed { error, finished }) => {
            hover.push_str(&format!(
                "\nlast sync: failed at {}\n{error}",
                finished.with_timezone(&chrono::Local).format("%H:%M:%S %Z")
            ));
        }
        Some(FluxSyncState::Running { phase, .. }) => {
            hover.push_str(&format!("\nsync running: {phase}"));
        }
        None => {}
    }

    let button = egui::Button::new(job).fill(Colors::bg_elevated(mode));
    let response = ui
        .add_enabled(enabled, button)
        .on_hover_text(&hover)
        .on_disabled_hover_text(&hover);
    if !enabled {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(500));
    }
    response.clicked()
}

fn button_text(
    icon: Option<(char, egui::Color32)>,
    text: &str,
    color: egui::Color32,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    if let Some((glyph, icon_color)) = icon {
        job.append(
            &glyph.to_string(),
            0.0_f32,
            egui::TextFormat {
                font_id: egui::FontId::new(9.0_f32, egui::FontFamily::Name(icons::FA_SOLID.into())),
                color: icon_color,
                valign: egui::Align::Center,
                ..Default::default()
            },
        );
    }
    job.append(
        text,
        if icon.is_some() { 6.0_f32 } else { 0.0_f32 },
        egui::TextFormat {
            font_id: egui::FontId::proportional(12.0_f32),
            color,
            valign: egui::Align::Center,
            ..Default::default()
        },
    );
    job
}

fn plural(n: u64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// `main@sha1:abcdef...` -> `main@abcdef12`
fn short_revision(revision: &str) -> String {
    match revision.split_once("sha1:") {
        Some((prefix, sha)) => format!("{prefix}{}", &sha[..sha.len().min(8)]),
        None => revision.chars().take(8).collect(),
    }
}

fn short_error(error: &str) -> String {
    let first = error.lines().next().unwrap_or(error);
    if first.chars().count() > 40 {
        format!("{}…", first.chars().take(40).collect::<String>())
    } else {
        first.to_string()
    }
}

fn ago(d: chrono::Duration) -> String {
    let s = d.num_seconds().max(0);
    if s < 60 {
        format!("{s}s ago")
    } else {
        format!("{}m ago", s / 60)
    }
}

fn promote_button(
    ui: &mut egui::Ui,
    state: &AppState,
    config: &Config,
    mode: ThemeMode,
    runtime: &tokio::runtime::Runtime,
) {
    let mirror_present = mirror_present(ui.ctx(), config);
    let passive = !state.run_mode().is_active();
    let reason = if !mirror_present {
        Some("no app-gitops mirror yet; wait for the first gitops poll")
    } else if passive {
        Some("passive instance: the active caravan owns GitHub; promote from there")
    } else {
        None
    };
    let color = if reason.is_some() {
        Colors::text_muted(mode)
    } else {
        Colors::ORANGE
    };
    let (from, to) = config
        .promote
        .pair(&config.environments)
        .map(|(f, t)| (env_label(&f), env_label(&t)))
        .unwrap_or_else(|| ("?".to_string(), "?".to_string()));
    let button = egui::Button::new(
        egui::RichText::new(format!("{from} → {to}"))
            .size(12.0_f32)
            .color(color),
    )
    .fill(Colors::bg_elevated(mode));
    let response = ui.add_enabled(reason.is_none(), button);
    if let Some(reason) = reason {
        response.on_disabled_hover_text(reason);
    } else if response
        .on_hover_text(format!("preview and push {from} pins to {to}"))
        .clicked()
    {
        crate::ui::promote::open(ui.ctx(), state, config, runtime);
    }
}

fn mirror_present(ctx: &egui::Context, config: &Config) -> bool {
    let id = egui::Id::new(MIRROR_PRESENT_ID);
    let cached = ctx.data_mut(|d| d.get_temp::<(Instant, bool)>(id));
    if let Some((at, present)) = cached {
        if at.elapsed().as_secs() < 5 {
            return present;
        }
    }
    let present =
        Mirror::open_existing(&crate::config::data_dir(), &config.github.app_gitops_remote)
            .is_some();
    ctx.data_mut(|d| d.insert_temp(id, (Instant::now(), present)));
    present
}

/// Once a minute, count the commits main has past each env's applied
/// kustomization revision (git rev-list on the local mirror).
fn maybe_spawn_distance_task(
    ctx: &egui::Context,
    state: &AppState,
    config: &Config,
    runtime: &tokio::runtime::Runtime,
) {
    let id = egui::Id::new(DISTANCE_TASK_ID);
    let last = ctx.data_mut(|d| d.get_temp::<Instant>(id));
    if last.is_some_and(|t| t.elapsed().as_secs() < DISTANCE_INTERVAL_SECS) {
        return;
    }
    ctx.data_mut(|d| d.insert_temp(id, Instant::now()));

    let applied: Vec<(String, String)> = config
        .environments
        .iter()
        .map(|e| &e.id)
        .filter_map(|env| {
            let sha = state
                .get_environment(env)
                .and_then(|e| e.kustomization)
                .and_then(|k| k.applied_sha)?;
            Some((env.clone(), sha))
        })
        .collect();
    if applied.is_empty() {
        return;
    }
    let head = state
        .get_gitops_head()
        .map(|h| h.sha)
        .unwrap_or_else(|| "main".to_string());
    let remote = config.github.app_gitops_remote.clone();
    let state = state.clone();
    runtime.spawn_blocking(move || {
        let Some(mirror) = Mirror::open_existing(&crate::config::data_dir(), &remote) else {
            return;
        };
        for (env, sha) in applied {
            match mirror.commits_between(&sha, &head) {
                Ok(n) => state.set_applied_distance(&env, n),
                Err(e) => tracing::debug!(env = %env, err = %e, "applied distance unavailable"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_shortens_to_branch_and_eight_hex() {
        assert_eq!(
            short_revision("main@sha1:0123456789abcdef0123456789abcdef01234567"),
            "main@01234567"
        );
        assert_eq!(short_revision("unknown revision"), "unknown ");
    }

    #[test]
    fn ago_switches_to_minutes() {
        assert_eq!(ago(chrono::Duration::seconds(42)), "42s ago");
        assert_eq!(ago(chrono::Duration::seconds(190)), "3m ago");
    }
}
