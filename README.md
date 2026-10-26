# Caravan

A desktop GitOps console for Flux-managed Kubernetes environments: what is declared in your gitops repo, what is running on each cluster, what is releasing, behind or ahead, and one-click sync, promote and vault unseal. Built with Rust and [egui](https://github.com/emilk/egui).

## Features

- **Status home**: problems strip and an app status matrix with releasing, behind, ahead and off-main markers
- **Flux sync**: one-click sync-with-source per environment from the top bar, with a sync-needed indicator
- **Promote**: copy an upstream environment's pins to a downstream one with a change-set preview, then commit and push from a scratch clone
- **Vault**: sealed banner on every view, one-click unseal with keys from a command or the OS keyring, desktop notification on seal
- **Libraries**: track releases, auto-hide stale repos, public/private visibility
- **Pull Requests and Issues**: polled only while the view is open
- **Gitops mirror**: declared versions read from a local bare git mirror, no GitHub API cost
- **Passive mode**: a second instance (or `CARAVAN_GITHUB=off`) runs without GitHub polling
- **SQLite cache**: instant startup from cached data
- **GitHub OAuth**: device flow authentication against your own OAuth App, token stored securely
- **Kubeconfig discovery**: auto-detects cluster configs, supports multi-context files
- **First-run wizard** and a **Settings page** for auth, clusters and config

## Installation

### Download a release

Download the latest binary for your platform from [Releases](../../releases):

| Platform | Binary |
|----------|--------|
| Linux x86_64 | `caravan-linux-x86_64.tar.gz` |
| macOS Apple Silicon | `caravan-macos-aarch64.dmg` |

### Build from source

Requires Rust stable and system dependencies for the GUI:

```bash
# Ubuntu/Debian
sudo apt-get install libgtk-3-dev libxdo-dev libdbus-1-dev

# macOS: no extra deps needed

cargo build --release
./target/release/caravan
```

## First Run

On first launch, Caravan opens a setup wizard:

1. **GitHub**: enter your organization and the client id of your own GitHub OAuth App (device flow enabled; register one under your organization's developer settings), then authenticate via the device flow. Or skip and use `gh auth token`.
2. **Kubernetes**: auto-discovers kubeconfig files, assign each to an environment and a type (apps/vault/infra)
3. **Done**: config is saved and the dashboard launches

You can revisit these settings anytime from the **Settings** page (gear icon in the sidebar).

## Configuration

Config lives at `~/.config/caravan/config.toml` (Linux) or the platform-appropriate config directory. Every key below has a default except `github.org`; the example shows a fictional organization `notes-co` with three environments and three services.

```toml
[github]
org = "notes-co"
# The client id of the OAuth App this deployment registered (device flow
# enabled). No default: each deployment registers its own.
oauth_client_id = "Iv1.0123456789abcdef"
# Fallbacks when no OAuth token is stored:
# token_cmd = "gh auth token"
# token_env = "GH_TOKEN"
gitops_repo = "gitops"                       # default "gitops"
# gitops_remote = "git@github.com:notes-co/gitops.git"   # default derived from org + gitops_repo
action_repo_prefix = "action-"               # repos named like this are GitHub Actions
release_pr_prefix = "Release "               # release PR titles: "Release notes-api v1.2.3"

[flux]
namespace = "flux-system"                    # where the GitRepository and Kustomization live
git_repository = "flux-system"               # GitRepository synced before a Kustomization
kustomization = "apps"                       # the Kustomization that applies the gitops repo

[kubeconfigs]
globs = ["~/.kube/*.yaml", "~/.kube/*.yml"]  # besides ~/.kube/config and $KUBECONFIG

[update]
# repo = "notes-co/caravan"                  # GitHub releases to self-update from; unset = off

[urls]
service_pattern = "https://{service}.{env}.example.com"
vault_pattern = "https://vault.{env}.example.com:8200"

# Environments in display order. `upstream` names the environment this one
# promotes from; `reference = true` marks the one whose failures matter most.
[[environments]]
id = "prod"
label = "Production"
color = "#F44336"
reference = true
kubeconfig = "~/.kube/prod.yaml"

[[environments]]
id = "staging"
label = "Staging"
color = "#FF9800"
upstream = "prod"
kubeconfig = "~/.kube/staging.yaml"

[[environments]]
id = "dev"
label = "Dev"
color = "#4CAF50"
upstream = "staging"
kubeconfig = "~/.kube/dev.yaml"
context = "admin@dev"                        # optional, for multi-context files

[vault.dev]
url = "https://vault.dev.example.com:8200"
kubeconfig = "~/.kube/dev-vault.yaml"
namespace = "vault"                                       # default
label_selector = "app.kubernetes.io/name=vault,component=server"   # default
# unseal_keys_cmd prints one unseal key per line
# unseal_keys_cmd = "~/.local/bin/vault-keys dev"
remember_keys = false
auto_unseal = false

[repos]
hide = []
# service_names = { "notes-frontend" = "notes-web" }    # repo -> deployed service name

[promote]
# enabled = false                            # no promote button or modal; default true
sync_after_push = true

[openobserve]
# url = "https://logs.example.com"           # unset: no external log links
org = "default"

[notifications]
enabled = true

[polling]
k8s_interval_secs = 10
github_interval_secs = 120
github_releases_interval_secs = 900
gitops_interval_secs = 120
vault_interval_secs = 30
```

### Token Resolution Order

GitHub tokens are resolved in this order:

1. Secure file storage (`~/.local/share/caravan/github-token`)
2. System keyring (when available)
3. `token_cmd` (e.g., `gh auth token`)
4. `token_env` environment variable
5. `token` literal in config

## Branding a build

Where config and data live must be known before config.toml is read, so the brand is fixed at compile time. Set these environment variables when building to ship a differently named build (all optional):

| Variable | Default | Used for |
|----------|---------|----------|
| `CARAVAN_BRAND_APP_NAME` | `caravan` | config and data directory names, keyring service, app id |
| `CARAVAN_BRAND_DISPLAY_NAME` | `Caravan` | window title, macOS `.app` name |
| `CARAVAN_BRAND_QUALIFIER` | `dev` | ProjectDirs qualifier |
| `CARAVAN_BRAND_ORG` | `caravan` | ProjectDirs organization |
| `CARAVAN_BRAND_BUNDLE_ID` | `dev.caravan.app` | macOS bundle identifier |
| `CARAVAN_BRAND_VOLUME_NAME` | `Caravan` | DMG volume the self-updater mounts |

Replace `assets/icon.png`, `assets/logo.png` and `assets/logomark.png` to change the marks.

## Development

```bash
# Run with debug logging
RUST_LOG=caravan=debug cargo run

# Run tests
cargo test

# Check for issues
cargo clippy --all-targets -- -D warnings
```

## License

Apache License 2.0, copyright 2026 Spice Labs, Inc. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

Bundled icons are from [Font Awesome Free 6](https://fontawesome.com): icons under CC BY 4.0, fonts under the SIL Open Font License 1.1, code under MIT.
