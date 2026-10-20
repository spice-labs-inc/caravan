//! Desktop notifications through the platform notification service.

use crate::config::NotificationConfig;
use crate::models::environments;

/// Whether notifications about `env` are wanted at all.
pub fn env_enabled(cfg: &NotificationConfig, env: &str) -> bool {
    if !cfg.enabled {
        return false;
    }
    cfg.failures_enabled(env, &environments::current())
}

/// Show a desktop notification. Gated by `[notifications] enabled`; the
/// per-env gate is `env_enabled`. Runs on its own thread because the
/// notification service call blocks.
pub fn notify(cfg: &NotificationConfig, title: &str, body: &str) {
    if !cfg.enabled {
        return;
    }
    let (title, body) = (title.to_string(), body.to_string());
    std::thread::spawn(move || {
        let result = notify_rust::Notification::new()
            .appname(crate::branding::DISPLAY_NAME)
            .summary(&title)
            .body(&body)
            .show();
        if let Err(e) = result {
            tracing::warn!(err = %e, "Desktop notification failed");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_gate_follows_config_and_ladder() {
        let cfg = NotificationConfig::default();
        assert!(env_enabled(&cfg, "prod"));
        assert!(env_enabled(&cfg, "staging"));
        assert!(
            !env_enabled(&cfg, "dev"),
            "the bottom rung is quiet by default"
        );
        assert!(env_enabled(&cfg, "other"));
        let mut loud = NotificationConfig::default();
        loud.failures.insert("dev".to_string(), true);
        assert!(env_enabled(&loud, "dev"));
        let off = NotificationConfig {
            enabled: false,
            ..NotificationConfig::default()
        };
        assert!(!env_enabled(&off, "prod"));
    }
}
