// The RayOps catalog: who is signed in, and what the gateway says they can reach.
//
// Held by the workspace rather than by the modal. The modal is a short-lived input surface; the
// catalog is what the session manager and the welcome screen read to show managed assets beside
// local connections. Keeping one copy also means the user authenticates once per run instead of
// once per surface that wants to show an asset.
//
// Nothing here is persisted. The token exists so that a second surface does not have to ask for a
// password, not so that a restart can skip the login: a deployment that is unreachable, or a token
// the gateway has revoked, must surface as "signed out" rather than as a stale list.

use gpui::Context;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

/// How long a token is trusted locally.
///
/// The gateway issues a 24-hour JWT, so this mirrors it. It is a ceiling, not a measurement: a
/// shorter server-side expiry is not observable from the token alone, which is why a rejected call
/// has to move the state to `Expired` rather than relying on this.
const SESSION_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

/// Where the catalog is in its lifecycle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::workspace) enum RayOpsCatalogPhase {
    /// No session. The UI offers a sign-in entry point.
    SignedOut,
    /// A sign-in is in flight.
    ///
    /// Distinct from `Refreshing`: a refresh already holds a token, so the tree can keep showing
    /// the previous list while this state has nothing to show yet.
    #[allow(dead_code)]
    Authenticating,
    /// A session exists and the list below is what the gateway last returned.
    Ready,
    /// A refresh is in flight; the previous list is still shown.
    Refreshing,
    /// The gateway rejected the token, or the local ceiling passed.
    Expired,
    /// The deployment could not be reached. The session is kept: a network blip should not sign
    /// the user out.
    Disconnected,
}

/// An authenticated session.
pub(in crate::workspace) struct RayOpsCatalogSession {
    pub(in crate::workspace) token: oxideterm_rayops::Secret,
    /// When this client stops trusting the token, whichever is earlier: the local ceiling or the
    /// moment the session was established plus that ceiling.
    expires_at: Instant,
}

/// Why the catalog is not showing assets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::workspace) struct RayOpsCatalogError {
    pub(in crate::workspace) message: String,
    /// Whether retrying could help. A refused token needs a new sign-in; a timeout does not.
    pub(in crate::workspace) retryable: bool,
}

/// The catalog itself.
pub(in crate::workspace) struct RayOpsCatalog {
    pub(in crate::workspace) phase: RayOpsCatalogPhase,
    /// The deployment the session belongs to. Compared before anything is reused, so a token for
    /// one deployment is never offered to another.
    pub(in crate::workspace) base_url: String,
    session: Option<RayOpsCatalogSession>,
    pub(in crate::workspace) groups: Vec<oxideterm_rayops::AssetGroup>,
    pub(in crate::workspace) assets: Vec<oxideterm_rayops::Asset>,
    pub(in crate::workspace) error: Option<RayOpsCatalogError>,
    /// Bumped per request so a reply from an abandoned search or refresh is recognised as stale.
    generation: u64,
}

impl Default for RayOpsCatalog {
    fn default() -> Self {
        Self {
            phase: RayOpsCatalogPhase::SignedOut,
            base_url: String::new(),
            session: None,
            groups: Vec::new(),
            assets: Vec::new(),
            error: None,
            generation: 0,
        }
    }
}

impl std::fmt::Debug for RayOpsCatalog {
    /// Prints the catalog without its token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RayOpsCatalog")
            .field("phase", &self.phase)
            .field("base_url", &self.base_url)
            .field("session", &self.session.as_ref().map(|_| "[REDACTED]"))
            .field("groups", &self.groups.len())
            .field("assets", &self.assets.len())
            .field("error", &self.error)
            .finish()
    }
}

impl RayOpsCatalog {
    /// Whether a signed-in session exists for `base_url`.
    ///
    /// Also the place a passed ceiling turns into `Expired`, so no caller has to remember to check
    /// the clock itself.
    pub(in crate::workspace) fn signed_in_for(&mut self, base_url: &str) -> bool {
        if self.base_url != base_url {
            return false;
        }
        let Some(session) = self.session.as_ref() else {
            return false;
        };
        if Instant::now() >= session.expires_at {
            self.clear_session(RayOpsCatalogPhase::Expired);
            self.error = Some(RayOpsCatalogError {
                message: "The RayOps session expired. Sign in again to see managed assets."
                    .to_owned(),
                retryable: false,
            });
            return false;
        }
        true
    }

    pub(in crate::workspace) fn token(&self) -> Option<oxideterm_rayops::Secret> {
        self.session.as_ref().map(|session| session.token.clone())
    }

    /// Records a successful sign-in.
    pub(in crate::workspace) fn adopt_session(
        &mut self,
        base_url: String,
        token: oxideterm_rayops::Secret,
    ) {
        self.base_url = base_url;
        self.session = Some(RayOpsCatalogSession {
            token,
            expires_at: Instant::now() + SESSION_LIFETIME,
        });
        self.phase = RayOpsCatalogPhase::Ready;
        self.error = None;
    }

    /// Replaces the asset list. Only the newest request may do so.
    pub(in crate::workspace) fn adopt_catalog(
        &mut self,
        generation: u64,
        groups: Vec<oxideterm_rayops::AssetGroup>,
        assets: Vec<oxideterm_rayops::Asset>,
    ) -> bool {
        if !self.is_current(generation) {
            return false;
        }
        self.groups = groups;
        self.assets = assets;
        self.phase = RayOpsCatalogPhase::Ready;
        self.error = None;
        true
    }

    /// The next generation, for tagging a request whose reply may be stale.
    pub(in crate::workspace) fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    fn is_current(&self, generation: u64) -> bool {
        generation == self.generation
    }

    /// Reports a failure, keeping the session unless the gateway refused the token.
    pub(in crate::workspace) fn report_failure(&mut self, error: RayOpsCatalogError) {
        if error.retryable {
            self.phase = RayOpsCatalogPhase::Disconnected;
        } else {
            // An auth failure is the one case where keeping the token is wrong: it would be
            // re-offered on every retry and fail the same way.
            self.clear_session(RayOpsCatalogPhase::Expired);
        }
        self.error = Some(error);
    }

    /// Forgets the session and everything it produced.
    pub(in crate::workspace) fn clear_session(&mut self, phase: RayOpsCatalogPhase) {
        self.session = None;
        self.groups.clear();
        self.assets.clear();
        self.phase = phase;
    }

    /// A signed-out catalog with no trace of the previous one.
    ///
    /// The UI has no sign-out control yet; it exists because the token has to be droppable on
    /// demand, and a menu entry is a smaller change than adding this later would be.
    #[allow(dead_code)]
    pub(in crate::workspace) fn sign_out(&mut self) {
        self.clear_session(RayOpsCatalogPhase::SignedOut);
        self.error = None;
    }
}

/// The password for a sign-in, from the environment.
///
/// A stand-in for the login form. Kept beside the catalog so the source of this credential is
/// stated once: it is read, used to build one request, and never stored.
pub(in crate::workspace) fn credential_from_environment() -> Result<Zeroizing<String>, String> {
    match std::env::var("RAYOPS_PASSWORD") {
        Ok(value) if !value.is_empty() => Ok(Zeroizing::new(value)),
        Ok(_) => Err("RAYOPS_PASSWORD is set but empty".to_owned()),
        Err(_) => Err(
            "RAYOPS_PASSWORD is not set. The password is read from the environment because storing \
             it would put a server credential in a settings file that is exported and synced."
                .to_owned(),
        ),
    }
}

#[cfg(test)]
mod rayops_catalog_tests {
    use super::*;

    fn catalog_signed_in() -> RayOpsCatalog {
        let mut catalog = RayOpsCatalog::default();
        catalog.adopt_session(
            "https://rayops.example".to_owned(),
            oxideterm_rayops::Secret::new("token-must-not-appear"),
        );
        catalog
    }

    /// A debug dump must not carry the token.
    #[test]
    fn debug_output_carries_no_token() {
        let rendered = format!("{:?}", catalog_signed_in());
        assert!(!rendered.contains("token-must-not-appear"), "{rendered}");
        assert!(rendered.contains("rayops.example"), "{rendered}");
    }

    /// A session belongs to one deployment and is not offered to another.
    #[test]
    fn a_session_is_not_reused_for_a_different_deployment() {
        let mut catalog = catalog_signed_in();
        assert!(catalog.signed_in_for("https://rayops.example"));
        assert!(
            !catalog.signed_in_for("https://other.example"),
            "a token for one deployment must not be presented to another"
        );
    }

    /// Signing out leaves nothing behind.
    #[test]
    fn signing_out_clears_the_token_and_the_catalog() {
        let mut catalog = catalog_signed_in();
        catalog.adopt_catalog(0, Vec::new(), Vec::new());
        catalog.sign_out();
        assert_eq!(catalog.phase, RayOpsCatalogPhase::SignedOut);
        assert!(catalog.token().is_none());
        assert!(catalog.assets.is_empty());
    }

    /// A refused token must not be kept for the next attempt.
    #[test]
    fn a_refused_token_is_dropped() {
        let mut catalog = catalog_signed_in();
        catalog.report_failure(RayOpsCatalogError {
            message: "unauthorized".to_owned(),
            retryable: false,
        });
        assert_eq!(catalog.phase, RayOpsCatalogPhase::Expired);
        assert!(catalog.token().is_none(), "retrying with it would fail the same way");
    }

    /// A transient failure keeps the session: a network blip should not sign the user out.
    #[test]
    fn a_transient_failure_keeps_the_session() {
        let mut catalog = catalog_signed_in();
        catalog.report_failure(RayOpsCatalogError {
            message: "timed out".to_owned(),
            retryable: true,
        });
        assert_eq!(catalog.phase, RayOpsCatalogPhase::Disconnected);
        assert!(catalog.token().is_some());
    }

    /// A reply from a superseded request must not replace a newer list.
    #[test]
    fn a_stale_reply_is_refused() {
        let mut catalog = catalog_signed_in();
        let first = catalog.next_generation();
        let second = catalog.next_generation();
        assert!(!catalog.adopt_catalog(first, Vec::new(), Vec::new()));
        assert!(catalog.adopt_catalog(second, Vec::new(), Vec::new()));
    }
}

impl crate::workspace::WorkspaceApp {
    /// Reloads the catalog using the session already held.
    ///
    /// Refreshing rather than signing in again: the token is what the catalog exists to keep, so a
    /// refresh that asked for a password would defeat it.
    pub(in crate::workspace) fn refresh_rayops_catalog(&mut self, cx: &mut Context<Self>) {
        let Some(token) = self.rayops_catalog.token() else {
            return;
        };
        let settings = self.settings_store.settings().rayops.clone();
        let base_url = self.rayops_catalog.base_url.clone();
        if base_url.is_empty() {
            return;
        }
        let generation = self.rayops_catalog.next_generation();
        self.rayops_catalog.phase = RayOpsCatalogPhase::Refreshing;
        cx.notify();

        let runtime = self.forwarding_runtime.clone();
        let insecure_tls = settings.insecure_tls;
        let allow_plaintext = settings.allow_plaintext;
        let task = cx.spawn(async move |this, cx| {
            let outcome = runtime
                .spawn(async move {
                    let (assets, groups) = tokio::join!(
                        crate::workspace::rayops_flow::fetch_assets(
                            crate::workspace::rayops_flow::RayOpsAssetRequest {
                                base_url: base_url.clone(),
                                insecure_tls,
                                allow_plaintext,
                                token: token.clone(),
                                keyword: String::new(),
                                page: 1,
                                size: 500,
                            }
                        ),
                        crate::workspace::rayops_flow::fetch_asset_groups(
                            base_url,
                            insecure_tls,
                            allow_plaintext,
                            token,
                        )
                    );
                    (assets, groups)
                })
                .await;
            let _ = this.update_in(cx, |this, _window, cx| {
                this.apply_rayops_catalog_refresh(generation, outcome, cx);
            });
        });
        task.detach();
    }

    fn apply_rayops_catalog_refresh(
        &mut self,
        generation: u64,
        outcome: Result<
            (
                Result<oxideterm_rayops::AssetPage, String>,
                Result<Vec<oxideterm_rayops::AssetGroup>, String>,
            ),
            tokio::task::JoinError,
        >,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            Ok((Ok(page), groups)) => {
                // A failing group fetch must not hide the assets: they are still reachable, and the
                // tree falls back to a flat list without headings.
                let groups = groups.unwrap_or_default();
                self.rayops_catalog.adopt_catalog(generation, groups, page.assets);
            }
            Ok((Err(message), _)) => {
                let retryable = !message.to_ascii_lowercase().contains("unauthorized")
                    && !message.to_ascii_lowercase().contains("401");
                self.rayops_catalog.report_failure(RayOpsCatalogError {
                    message,
                    retryable,
                });
            }
            Err(join) => {
                self.rayops_catalog.report_failure(RayOpsCatalogError {
                    message: format!("the RayOps refresh failed: {join}"),
                    retryable: true,
                });
            }
        }
        cx.notify();
    }

    /// Connects to a managed asset picked from the tree.
    ///
    /// Runs the whole flow — precheck, ticket, handshake — because the tree shows the same catalog
    /// the picker does, and a row there is an offer to connect, not a saved connection that can be
    /// reopened without authorization.
    pub(in crate::workspace) fn connect_rayops_catalog_asset(
        &mut self,
        asset_id: i64,
        cx: &mut Context<Self>,
    ) {
        let Some(token) = self.rayops_catalog.token() else {
            return;
        };
        let settings = self.settings_store.settings().rayops.clone();
        let base_url = self.rayops_catalog.base_url.clone();
        if base_url.is_empty() {
            return;
        }
        let password = match credential_from_environment() {
            Ok(password) => password,
            Err(message) => {
                self.notify_rayops_error(message, cx);
                return;
            }
        };
        let title = self
            .rayops_catalog
            .assets
            .iter()
            .find(|asset| asset.id == asset_id)
            .map(|asset| asset.hostname.clone())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| format!("RayOps {asset_id}"));

        let launch = crate::workspace::rayops_flow::RayOpsLaunch {
            base_url,
            asset_id,
            insecure_tls: settings.insecure_tls,
            allow_plaintext: settings.allow_plaintext,
            username: std::env::var("RAYOPS_USERNAME").unwrap_or_else(|_| "admin".to_owned()),
            password,
        };
        let runtime = self.forwarding_runtime.clone();
        let terminal_options = oxideterm_connections::ConnectionTerminalOptions::default();
        let generation = self.rayops_catalog.next_generation();
        let _ = token;

        let task = cx.spawn(async move |this, cx| {
            let outcome = runtime
                .spawn(crate::workspace::rayops_flow::open_rayops_connection(launch))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                match outcome {
                    Ok(Ok(connection)) => {
                        if let Err(error) = this.create_rayops_terminal_tab(
                            title,
                            connection.socket,
                            terminal_options,
                            window,
                            cx,
                        ) {
                            this.notify_rayops_error(error.to_string(), cx);
                        }
                    }
                    Ok(Err(message)) => {
                        // A refusal is the one failure the catalog has to act on, because keeping a
                        // token the gateway will keep rejecting makes every retry fail the same way.
                        let lowered = message.to_ascii_lowercase();
                        if lowered.contains("denied") || lowered.contains("approval") {
                            this.rayops_catalog.report_failure(RayOpsCatalogError {
                                message: message.clone(),
                                retryable: false,
                            });
                            cx.notify();
                        }
                        this.notify_rayops_error(message, cx);
                    }
                    Err(join) => {
                        this.notify_rayops_error(format!("the RayOps connection failed: {join}"), cx)
                    }
                }
                let _ = generation;
            });
        });
        task.detach();
    }
}

impl crate::workspace::WorkspaceApp {
    /// Signs in at start-up when the deployment and a credential are both available.
    ///
    /// Without this the session manager shows nothing until the user opens the picker, which reads
    /// as "the assets disappeared" rather than as "nobody is signed in". A missing credential is
    /// not an error here: it is the ordinary state for someone who has not configured a
    /// deployment, and the tree shows a sign-in entry instead.
    pub(in crate::workspace) fn start_rayops_catalog_session(&mut self, cx: &mut Context<Self>) {
        let settings = self.settings_store.settings().rayops.clone();
        let base_url = settings.base_url.trim().to_owned();
        if base_url.is_empty() {
            return;
        }
        if credential_from_environment().is_err() {
            // Reported by the tree as "sign in", not as a failure: the credential source is
            // provisional and its absence is not something the user did wrong.
            return;
        }
        self.authenticate_rayops_catalog(cx);
    }

    /// Authenticates the catalog, if it is not already signed in.
    pub(in crate::workspace) fn authenticate_rayops_catalog(&mut self, cx: &mut Context<Self>) {
        let settings = self.settings_store.settings().rayops.clone();
        let base_url = settings.base_url.trim().to_owned();
        if base_url.is_empty() {
            return;
        }
        if self.rayops_catalog.phase == RayOpsCatalogPhase::Authenticating {
            return;
        }
        let password = match credential_from_environment() {
            Ok(password) => password,
            Err(message) => {
                self.rayops_catalog.report_failure(RayOpsCatalogError {
                    message,
                    retryable: false,
                });
                cx.notify();
                return;
            }
        };
        self.rayops_catalog.phase = RayOpsCatalogPhase::Authenticating;
        self.rayops_catalog.error = None;
        cx.notify();

        let launch = crate::workspace::rayops_flow::RayOpsLaunch {
            base_url: base_url.clone(),
            asset_id: 0,
            insecure_tls: settings.insecure_tls,
            allow_plaintext: settings.allow_plaintext,
            username: std::env::var("RAYOPS_USERNAME").unwrap_or_else(|_| "admin".to_owned()),
            password,
        };
        let runtime = self.forwarding_runtime.clone();
        let task = cx.spawn(async move |this, cx| {
            let outcome = runtime
                .spawn(crate::workspace::rayops_flow::sign_in(launch))
                .await;
            let _ = this.update_in(cx, |this, _window, cx| {
                match outcome {
                    Ok(Ok(session)) => {
                        this.rayops_catalog.adopt_session(base_url, session.token);
                        this.refresh_rayops_catalog(cx);
                    }
                    Ok(Err(message)) => {
                        let lowered = message.to_ascii_lowercase();
                        // A refused credential is not retryable with the same one, so it must not
                        // be presented as a transient failure.
                        let retryable = !lowered.contains("unauthorized")
                            && !lowered.contains("401")
                            && !lowered.contains("invalid");
                        this.rayops_catalog.report_failure(RayOpsCatalogError {
                            message,
                            retryable,
                        });
                        cx.notify();
                    }
                    Err(join) => {
                        this.rayops_catalog.report_failure(RayOpsCatalogError {
                            message: format!("the RayOps sign-in failed: {join}"),
                            retryable: true,
                        });
                        cx.notify();
                    }
                }
            });
        });
        task.detach();
    }
}
