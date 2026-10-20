//! Where vault unseal keys come from, in order: the per-env
//! `unseal_keys_cmd`, then keys remembered in the OS keyring (opt-in per
//! env), then manual entry. Keys are wrapped in `Zeroizing` so they are wiped
//! when dropped; they are never logged and never written to disk except
//! through the keyring when the user asked for that.

use anyhow::{Context, Result};
use zeroize::Zeroizing;

use crate::config::VaultConfig;

pub type UnsealKeys = Zeroizing<Vec<String>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Command,
    Keyring,
    Manual,
}

impl KeySource {
    pub fn label(&self) -> &'static str {
        match self {
            KeySource::Command => "command",
            KeySource::Keyring => "keyring",
            KeySource::Manual => "manual",
        }
    }

    /// Whether keys can be fetched without the user typing them.
    pub fn is_automatic(&self) -> bool {
        !matches!(self, KeySource::Manual)
    }
}

fn keyring_key(env: &str) -> String {
    format!("vault-unseal-{}", env)
}

/// Split command or keyring output into keys: one per line, trimmed, blanks
/// dropped.
pub fn parse_keys(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Run `unseal_keys_cmd` through the shell and collect its stdout.
fn keys_from_command(cmd: &str) -> Result<UnsealKeys> {
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .with_context(|| format!("Failed to run unseal_keys_cmd: {}", cmd))?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        anyhow::bail!(
            "unseal_keys_cmd exited with {}{}",
            output.status,
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {}", stderr)
            }
        );
    }
    let stdout =
        Zeroizing::new(String::from_utf8(output.stdout).context("Key output is not UTF-8")?);
    let keys = parse_keys(&stdout);
    if keys.is_empty() {
        anyhow::bail!("unseal_keys_cmd printed no keys");
    }
    Ok(Zeroizing::new(keys))
}

/// Whether the remembered-keys path applies: the config flag, or the
/// in-memory override set when the user clicks "Remember keys on this machine".
pub fn remember_enabled(cfg: &VaultConfig, override_flag: Option<bool>) -> bool {
    override_flag.unwrap_or(cfg.remember_keys)
}

/// Resolve keys for an env. `Ok(None)` means only manual entry is available.
/// Errors come from a configured source that failed, and never contain keys.
pub fn resolve_keys(
    env: &str,
    cfg: &VaultConfig,
    remember_override: Option<bool>,
) -> Result<Option<(UnsealKeys, KeySource)>> {
    if let Some(cmd) = cfg
        .unseal_keys_cmd
        .as_deref()
        .filter(|c| !c.trim().is_empty())
    {
        return keys_from_command(cmd).map(|k| Some((k, KeySource::Command)));
    }
    if remember_enabled(cfg, remember_override) {
        if let Some(text) = crate::auth::keyring::retrieve_secret(&keyring_key(env))? {
            let text = Zeroizing::new(text);
            let keys = parse_keys(&text);
            if !keys.is_empty() {
                return Ok(Some((Zeroizing::new(keys), KeySource::Keyring)));
            }
        }
    }
    Ok(None)
}

/// Which source `resolve_keys` would use, without running the command. The
/// keyring is read to learn whether an entry exists.
pub fn probe_key_source(
    env: &str,
    cfg: &VaultConfig,
    remember_override: Option<bool>,
) -> KeySource {
    if cfg
        .unseal_keys_cmd
        .as_deref()
        .is_some_and(|c| !c.trim().is_empty())
    {
        return KeySource::Command;
    }
    if remember_enabled(cfg, remember_override) {
        if let Ok(Some(text)) = crate::auth::keyring::retrieve_secret(&keyring_key(env)) {
            let text = Zeroizing::new(text);
            if !parse_keys(&text).is_empty() {
                return KeySource::Keyring;
            }
        }
    }
    KeySource::Manual
}

pub fn remember_keys_in_keyring(env: &str, keys: &[String]) -> Result<()> {
    let joined = Zeroizing::new(keys.join("\n"));
    crate::auth::keyring::store_secret(&keyring_key(env), &joined)
}

#[allow(dead_code)]
pub fn forget_keys(env: &str) -> Result<()> {
    crate::auth::keyring::delete_secret(&keyring_key(env))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(cmd: Option<&str>, remember: bool) -> VaultConfig {
        VaultConfig {
            url: "https://vault.example".to_string(),
            kubeconfig: None,
            unseal_keys_cmd: cmd.map(str::to_string),
            remember_keys: remember,
            auto_unseal: false,
            namespace: "vault".to_string(),
            label_selector: "app.kubernetes.io/name=vault".to_string(),
        }
    }

    #[test]
    fn parse_keys_trims_and_drops_blank_lines() {
        assert_eq!(parse_keys("a\n  b \n\n\nc\n"), vec!["a", "b", "c"]);
        assert!(parse_keys("\n \n").is_empty());
    }

    #[test]
    fn command_output_becomes_keys() {
        let c = cfg(Some("printf 'a\\nb\\n\\n'"), false);
        let (keys, source) = resolve_keys("test", &c, None).unwrap().unwrap();
        assert_eq!(source, KeySource::Command);
        assert_eq!(keys.as_slice(), ["a", "b"]);
    }

    #[test]
    fn command_stderr_goes_to_error_not_keys() {
        let c = cfg(Some("echo boom >&2; exit 3"), false);
        let err = resolve_keys("test", &c, None).unwrap_err().to_string();
        assert!(err.contains("boom"), "{}", err);
    }

    #[test]
    fn empty_command_output_is_an_error() {
        let c = cfg(Some("true"), false);
        assert!(resolve_keys("test", &c, None).is_err());
    }

    #[test]
    fn no_command_and_no_remember_is_manual() {
        let c = cfg(None, false);
        assert!(resolve_keys("test", &c, None).unwrap().is_none());
        assert_eq!(probe_key_source("test", &c, None), KeySource::Manual);
        assert_eq!(probe_key_source("test", &c, Some(false)), KeySource::Manual);
    }

    #[test]
    fn command_wins_over_keyring_in_probe() {
        let c = cfg(Some("echo k"), true);
        assert_eq!(probe_key_source("test", &c, None), KeySource::Command);
    }

    #[test]
    #[ignore = "needs an unlocked OS keyring"]
    fn keyring_round_trip() {
        let env = "caravan-unit-test";
        remember_keys_in_keyring(env, &["one".to_string(), "two".to_string()]).unwrap();
        let c = cfg(None, true);
        let (keys, source) = resolve_keys(env, &c, None).unwrap().unwrap();
        assert_eq!(source, KeySource::Keyring);
        assert_eq!(keys.as_slice(), ["one", "two"]);
        forget_keys(env).unwrap();
        assert!(resolve_keys(env, &c, None).unwrap().is_none());
    }
}
