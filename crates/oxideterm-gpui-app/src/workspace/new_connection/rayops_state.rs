// State for the RayOps login and asset-picking flow.
//
// Kept apart from the SSH form state because the two flows share nothing: this one holds a
// short-lived session token that must never reach the settings file, and it is a two-step flow
// (authenticate, then choose) rather than one form that is submitted once.
//
// The password lives here only until the login request has been built. It is a `Zeroizing` so a
// dropped state clears it, and it is deliberately not part of any struct that gets serialized:
// the settings file is exported, synced and backed up.

use zeroize::Zeroizing;

/// Which page of the asset list to request.
///
/// The gateway paginates, and the list is not small on a real deployment, so the page travels
/// with the search keyword rather than being recomputed from a scroll position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::workspace) struct RayOpsPage {
    pub(in crate::workspace) index: u32,
    pub(in crate::workspace) size: u32,
}

impl Default for RayOpsPage {
    fn default() -> Self {
        // Large enough that a typical deployment fits on one page, small enough that the gateway
        // is not asked for everything.
        Self {
            index: 1,
            size: 50,
        }
    }
}

/// A failure the user must resolve, with the distinction the gateway makes.
///
/// `Denied` and `ApprovalRequired` are separate because they call for different actions: one is
/// final, the other ends when someone approves. Collapsing them into "access denied" would tell
/// the user to give up when waiting would have worked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::workspace) enum RayOpsBlock {
    /// The gateway refused this identity outright.
    Denied { reason: String },
    /// Someone must approve this connection before it can start.
    ApprovalRequired { reason: String, approval_id: Option<String> },
    /// A transport, protocol or configuration failure.
    Failed { message: String },
}

impl RayOpsBlock {
    /// Classifies a failure message from the connection flow.
    ///
    /// The flow reports governance outcomes as text, so the distinction is recovered here rather
    /// than lost: telling a user to wait for approval and telling them they were refused are
    /// different instructions.
    pub(in crate::workspace) fn classify(message: String) -> Self {
        let lowered = message.to_ascii_lowercase();
        if lowered.contains("approval is required") {
            return Self::ApprovalRequired {
                reason: message,
                approval_id: None,
            };
        }
        if lowered.contains("denied") {
            return Self::Denied { reason: message };
        }
        Self::Failed { message }
    }

    pub(in crate::workspace) fn message(&self) -> String {
        match self {
            Self::Denied { reason } => format!("Access was denied: {reason}"),
            Self::ApprovalRequired { reason, approval_id } => match approval_id {
                Some(id) => format!("Approval required before connecting: {reason} (approval {id})"),
                None => format!("Approval required before connecting: {reason}"),
            },
            Self::Failed { message } => message.clone(),
        }
    }
}

/// What the modal is doing right now.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub(in crate::workspace) enum RayOpsPhase {
    /// Collecting credentials. Nothing has been sent.
    #[default]
    Credentials,
    /// A login request is in flight.
    Authenticating,
    /// Logged in; the asset list is on screen. `loading` covers a search or page change.
    Browsing { loading: bool },
}

/// The whole flow state.
#[derive(Clone)]
pub(in crate::workspace) struct RayOpsFlowState {
    pub(in crate::workspace) phase: RayOpsPhase,
    pub(in crate::workspace) base_url: String,
    pub(in crate::workspace) username: String,
    /// Cleared once the login request has been built, and on any exit from the flow.
    pub(in crate::workspace) password: Zeroizing<String>,
    pub(in crate::workspace) insecure_tls: bool,
    pub(in crate::workspace) allow_plaintext: bool,
    /// Held only while browsing. Never persisted, never logged, and dropped when the flow ends.
    pub(in crate::workspace) token: Option<oxideterm_rayops::Secret>,
    pub(in crate::workspace) assets: Vec<oxideterm_rayops::Asset>,
    pub(in crate::workspace) search: String,
    pub(in crate::workspace) page: RayOpsPage,
    /// Whether the gateway reported more pages after this one.
    pub(in crate::workspace) has_more: bool,
    pub(in crate::workspace) block: Option<RayOpsBlock>,
    /// Bumped per request so a slow reply from an abandoned search cannot overwrite a newer one.
    pub(in crate::workspace) request_generation: u64,
    /// Which field owns text input. `None` means no field is focused.
    pub(in crate::workspace) focused_field: Option<RayOpsField>,
}

impl Default for RayOpsFlowState {
    fn default() -> Self {
        Self {
            phase: RayOpsPhase::Credentials,
            base_url: String::new(),
            username: String::new(),
            password: Zeroizing::new(String::new()),
            insecure_tls: false,
            allow_plaintext: false,
            token: None,
            assets: Vec::new(),
            search: String::new(),
            page: RayOpsPage::default(),
            has_more: false,
            block: None,
            request_generation: 0,
            focused_field: None,
        }
    }
}

impl std::fmt::Debug for RayOpsFlowState {
    /// Prints the flow's position without its credentials.
    ///
    /// Written by hand because the derived version would print the password and the session
    /// token, and this type is reachable from the workspace entity that diagnostics dump.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RayOpsFlowState")
            .field("phase", &self.phase)
            .field("base_url", &self.base_url)
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("assets", &self.assets.len())
            .field("search", &self.search)
            .field("page", &self.page)
            .field("has_more", &self.has_more)
            .field("block", &self.block)
            .finish()
    }
}

impl RayOpsFlowState {
    /// A flow seeded from settings, ready for credentials.
    pub(in crate::workspace) fn from_settings(
        base_url: String,
        username: String,
        insecure_tls: bool,
        allow_plaintext: bool,
    ) -> Self {
        Self {
            base_url,
            username,
            insecure_tls,
            allow_plaintext,
            ..Self::default()
        }
    }

    /// Forgets everything that must not outlive a single attempt.
    ///
    /// Called when the flow ends for any reason — success, cancellation or failure — so a
    /// cancelled attempt cannot leave a live token in memory until the entity is dropped.
    pub(in crate::workspace) fn forget_credentials(&mut self) {
        self.password = Zeroizing::new(String::new());
        self.token = None;
    }

    /// The next generation, for tagging a request whose reply may be stale.
    pub(in crate::workspace) fn next_generation(&mut self) -> u64 {
        self.request_generation += 1;
        self.request_generation
    }

    /// Whether a reply tagged `generation` is still the newest one.
    pub(in crate::workspace) fn is_current(&self, generation: u64) -> bool {
        generation == self.request_generation
    }
}

#[cfg(test)]
mod rayops_state_tests {
    use super::*;

    /// A debug dump must not carry either credential.
    ///
    /// This state is reachable from a workspace entity that diagnostics format, and the derived
    /// `Debug` printed the password and the session token.
    #[test]
    fn debug_output_carries_no_credential() {
        let mut state = RayOpsFlowState::from_settings(
            "https://rayops.example".to_owned(),
            "admin".to_owned(),
            false,
            false,
        );
        state.password = Zeroizing::new("password-must-not-appear".to_owned());
        state.token = Some(oxideterm_rayops::Secret::new("token-must-not-appear"));

        let rendered = format!("{state:?}");
        assert!(!rendered.contains("password-must-not-appear"), "{rendered}");
        assert!(!rendered.contains("token-must-not-appear"), "{rendered}");
        // The fields that make the dump useful survive.
        assert!(rendered.contains("rayops.example"), "{rendered}");
        assert!(rendered.contains("admin"), "{rendered}");
    }

    /// Ending a flow must not leave credentials in memory.
    #[test]
    fn forgetting_clears_both_credentials() {
        let mut state = RayOpsFlowState::default();
        state.password = Zeroizing::new("secret".to_owned());
        state.token = Some(oxideterm_rayops::Secret::new("token"));

        state.forget_credentials();

        assert!(state.password.is_empty());
        assert!(state.token.is_none());
    }

    /// A reply from an abandoned search must be recognisable as stale.
    #[test]
    fn a_superseded_request_is_not_current() {
        let mut state = RayOpsFlowState::default();
        let first = state.next_generation();
        let second = state.next_generation();
        assert!(!state.is_current(first), "the older reply must be ignored");
        assert!(state.is_current(second));
    }

    /// Access outcomes that call for different actions stay distinguishable.
    #[test]
    fn denial_and_pending_approval_read_differently() {
        let denied = RayOpsBlock::Denied {
            reason: "outside your scope".to_owned(),
        };
        let pending = RayOpsBlock::ApprovalRequired {
            reason: "production asset".to_owned(),
            approval_id: Some("A-1".to_owned()),
        };
        assert!(denied.message().contains("denied"));
        assert!(pending.message().contains("Approval required"));
        assert!(pending.message().contains("A-1"));
        assert_ne!(denied.message(), pending.message());
    }
}

/// One editable field in the RayOps modal.
///
/// Separate from `NewConnectionField` because the two flows store their text in different places:
/// this one lives in `RayOpsFlowState`, whose password must never reach the SSH profile logic that
/// the connection form's fields are wired into.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(in crate::workspace) enum RayOpsField {
    BaseUrl,
    Username,
    Password,
    AssetSearch,
}

impl RayOpsField {
    /// Stable index used for element ids and anchor identity.
    pub(in crate::workspace) fn index(self) -> u32 {
        match self {
            Self::BaseUrl => 0,
            Self::Username => 1,
            Self::Password => 2,
            Self::AssetSearch => 3,
        }
    }

    /// The text for this field, for the IME snapshot.
    pub(in crate::workspace) fn value<'a>(self, state: &'a RayOpsFlowState) -> &'a str {
        match self {
            Self::BaseUrl => &state.base_url,
            Self::Username => &state.username,
            Self::Password => state.password.as_str(),
            Self::AssetSearch => &state.search,
        }
    }

    /// Mutable text for this field.
    pub(in crate::workspace) fn value_mut(self, state: &mut RayOpsFlowState) -> &mut String {
        match self {
            // `Zeroizing<String>` derefs to `String`, so the password edits in place like the rest
            // and the allocation is still scrubbed when the state drops.
            Self::Password => &mut state.password,
            Self::BaseUrl => &mut state.base_url,
            Self::Username => &mut state.username,
            Self::AssetSearch => &mut state.search,
        }
    }

    /// Whether the field renders its contents masked.
    pub(in crate::workspace) fn is_secret(self) -> bool {
        matches!(self, Self::Password)
    }
}
