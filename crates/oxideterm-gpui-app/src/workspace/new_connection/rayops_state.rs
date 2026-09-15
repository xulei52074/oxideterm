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
        // The whole estate by default. A picker that shows only part of the inventory makes the
        // user page through it to find an asset they can see exists, and the gateway answers 200
        // assets on a real deployment without complaint. Paging stays available for a larger one.
        Self {
            index: 1,
            size: 200,
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
    /// The gateway's tree, fetched once per session. Empty means the deployment has no groups,
    /// which the picker renders as a flat list.
    pub(in crate::workspace) groups: Vec<oxideterm_rayops::AssetGroup>,
    pub(in crate::workspace) search: String,
    pub(in crate::workspace) page: RayOpsPage,
    /// Whether the gateway reported more pages after this one.
    pub(in crate::workspace) has_more: bool,
    pub(in crate::workspace) block: Option<RayOpsBlock>,
    /// Bumped per request so a slow reply from an abandoned search cannot overwrite a newer one.
    pub(in crate::workspace) request_generation: u64,
    /// Which field owns text input. `None` means no field is focused.
    pub(in crate::workspace) focused_field: Option<RayOpsField>,
    /// Groups the user collapsed. Stored as collapsed rather than expanded so a group that
    /// appears after a refresh starts open — a newly visible group should not arrive hidden.
    pub(in crate::workspace) collapsed_groups: std::collections::HashSet<i64>,
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
            groups: Vec::new(),
            search: String::new(),
            page: RayOpsPage::default(),
            has_more: false,
            block: None,
            request_generation: 0,
            focused_field: None,
            collapsed_groups: std::collections::HashSet::new(),
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
    /// The token deliberately survives a closed modal (see `RayOpsSessionCache`), so this is not
    /// called on cancel. It exists for the case where the flow must be abandoned outright — the
    /// deployment changed, or the gateway rejected the token — and is the one place that clears
    /// both secrets at once.
    #[allow(dead_code)]
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


/// An authenticated RayOps session and the catalog it produced.
///
/// Held by the workspace rather than by the modal, because the alternative is what the first
/// version did: the token died with the dialog, so every visit asked for the password again. The
/// session is per-run process state — never written anywhere — and the catalog is only ever as
/// fresh as the last fetch, which the UI says out loud.
pub(in crate::workspace) struct RayOpsSessionCache {
    /// The deployment this session belongs to. A token for one deployment must not be offered to
    /// another, so the URL is compared before the cache is reused.
    pub(in crate::workspace) base_url: String,
    pub(in crate::workspace) token: oxideterm_rayops::Secret,
    /// Empty means never fetched, not "no assets".
    pub(in crate::workspace) groups: Vec<oxideterm_rayops::AssetGroup>,
    pub(in crate::workspace) assets: Vec<oxideterm_rayops::Asset>,
    pub(in crate::workspace) has_more: bool,
    /// Which page `assets` holds, so the next fetch continues from here.
    pub(in crate::workspace) page: u32,
}

impl std::fmt::Debug for RayOpsSessionCache {
    /// Prints the catalog without the token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RayOpsSessionCache")
            .field("base_url", &self.base_url)
            .field("token", &"[REDACTED]")
            .field("groups", &self.groups.len())
            .field("assets", &self.assets.len())
            .field("has_more", &self.has_more)
            .field("page", &self.page)
            .finish()
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

/// One node of the asset tree, assembled from the gateway's flat list.
///
/// Built here rather than in the view because a view cannot test it, and the shape the gateway
/// sends — a flat list with parent links — is the part most likely to be got wrong.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::workspace) struct RayOpsTreeNode {
    pub(in crate::workspace) group: oxideterm_rayops::AssetGroup,
    pub(in crate::workspace) children: Vec<RayOpsTreeNode>,
    /// Assets filed directly under this group.
    pub(in crate::workspace) assets: Vec<oxideterm_rayops::Asset>,
}

impl RayOpsTreeNode {
    /// Every asset in this subtree, including this group's own.
    pub(in crate::workspace) fn total_assets(&self) -> usize {
        self.assets.len() + self.children.iter().map(Self::total_assets).sum::<usize>()
    }
}

/// Builds the tree, then attaches assets to the deepest group that holds them.
///
/// An asset whose `group_id` names no group in the tree is placed at the root level rather than
/// dropped: an asset the user can connect to must never become invisible because its folder was
/// deleted or is not visible to this user.
pub(in crate::workspace) fn build_asset_tree(
    groups: &[oxideterm_rayops::AssetGroup],
    assets: &[oxideterm_rayops::Asset],
) -> Vec<RayOpsTreeNode> {
    use std::collections::HashMap;

    let mut by_parent: HashMap<i64, Vec<&oxideterm_rayops::AssetGroup>> = HashMap::new();
    for group in groups {
        by_parent.entry(group.parent_id).or_default().push(group);
    }
    for children in by_parent.values_mut() {
        children.sort_by_key(|group| (group.sort_order, group.id));
    }

    let known: std::collections::HashSet<i64> = groups.iter().map(|group| group.id).collect();
    let mut assets_by_group: HashMap<i64, Vec<oxideterm_rayops::Asset>> = HashMap::new();
    for asset in assets {
        let key = if known.contains(&asset.group_id) {
            asset.group_id
        } else {
            0
        };
        assets_by_group.entry(key).or_default().push(asset.clone());
    }

    fn build(
        parent: i64,
        by_parent: &HashMap<i64, Vec<&oxideterm_rayops::AssetGroup>>,
        assets_by_group: &mut HashMap<i64, Vec<oxideterm_rayops::Asset>>,
        depth: usize,
    ) -> Vec<RayOpsTreeNode> {
        // Depth-bounded: a malformed tree with a cycle would otherwise recurse until the stack
        // ends. Twelve levels is far past any real deployment.
        if depth > 12 {
            return Vec::new();
        }
        by_parent
            .get(&parent)
            .map(|children| {
                children
                    .iter()
                    .map(|group| RayOpsTreeNode {
                        group: (*group).clone(),
                        children: build(group.id, by_parent, assets_by_group, depth + 1),
                        assets: assets_by_group.remove(&group.id).unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    // Groups holding nothing, directly or below them, are dropped: an empty folder is noise in a
    // picker that exists to reach a machine. A user with 162 assets in one group and three empty
    // ones sees one heading instead of four.
    fn prune_empty(nodes: Vec<RayOpsTreeNode>) -> Vec<RayOpsTreeNode> {
        nodes
            .into_iter()
            .filter_map(|mut node| {
                node.children = prune_empty(node.children);
                let keep = !node.assets.is_empty() || !node.children.is_empty();
                keep.then_some(node)
            })
            .collect()
    }

    let mut roots = prune_empty(build(0, &by_parent, &mut assets_by_group, 0));
    // Anything left belongs to a group this user cannot see. Surfaced as an unnamed root-level
    // node so those assets stay reachable.
    let orphans: Vec<_> = assets_by_group.into_values().flatten().collect();
    if !orphans.is_empty() {
        roots.push(RayOpsTreeNode {
            group: oxideterm_rayops::AssetGroup {
                id: 0,
                name: String::new(),
                parent_id: 0,
                sort_order: i64::MAX,
                icon: String::new(),
                color: String::new(),
            },
            children: Vec::new(),
            assets: orphans,
        });
    }
    roots
}


    fn group(id: i64, name: &str, parent_id: i64) -> oxideterm_rayops::AssetGroup {
        oxideterm_rayops::AssetGroup {
            id,
            name: name.to_owned(),
            parent_id,
            sort_order: 0,
            icon: String::new(),
            color: String::new(),
        }
    }

    fn asset(id: i64, group_id: i64) -> oxideterm_rayops::Asset {
        oxideterm_rayops::Asset {
            id,
            hostname: format!("h{id}"),
            ip: String::new(),
            port: 22,
            platform: String::new(),
            protocols: Vec::new(),
            os: String::new(),
            group_id,
            tags: Vec::new(),
            credential_verify_status: String::new(),
        }
    }

    /// The flat list with parent links becomes a tree.
    #[test]
    fn the_tree_nests_children_under_their_parent() {
        let groups = vec![
            oxideterm_rayops::AssetGroup { id: 1, name: "prod".into(), parent_id: 0, sort_order: 0, icon: String::new(), color: String::new() },
            oxideterm_rayops::AssetGroup { id: 2, name: "db".into(), parent_id: 1, sort_order: 0, icon: String::new(), color: String::new() },
        ];
        // Both groups need an asset: an empty group is pruned, so a fixture without assets would
        // assert against a tree that is legitimately empty.
        let tree = build_asset_tree(&groups, &[asset(1, 1), asset(2, 2)]);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].group.name, "prod");
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(tree[0].children[0].group.name, "db");
    }

    /// An asset lands in its own group, not in its parent's.
    #[test]
    fn assets_are_filed_under_their_own_group() {
        let groups = vec![
            oxideterm_rayops::AssetGroup { id: 1, name: "prod".into(), parent_id: 0, sort_order: 0, icon: String::new(), color: String::new() },
            oxideterm_rayops::AssetGroup { id: 2, name: "db".into(), parent_id: 1, sort_order: 0, icon: String::new(), color: String::new() },
        ];
        let tree = build_asset_tree(&groups, &[asset(1, 2), asset(2, 1)]);
        assert_eq!(tree[0].assets.len(), 1, "the root group holds only its own asset");
        assert_eq!(tree[0].children[0].assets.len(), 1);
        assert_eq!(tree[0].total_assets(), 2);
    }

    /// An asset whose group is absent must stay reachable.
    #[test]
    fn an_asset_in_an_unknown_group_stays_visible() {
        let groups = vec![group(1, "prod", 0)];
        // The only asset names a group that is not in the tree, so the named group is pruned as
        // empty and the orphan takes its own root node.
        let orphan = oxideterm_rayops::Asset {
            group_id: 404,
            ..asset(9, 404)
        };
        let tree = build_asset_tree(&groups, &[orphan]);
        // The named group is empty once its only asset is found to belong elsewhere, so it is
        // pruned and the orphan is the sole root.
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].assets.len(), 1);
        assert_eq!(tree[0].assets[0].hostname, "h9");
        assert!(tree[0].group.name.is_empty(), "the orphan node carries no header");
    }

    /// A group holding nothing is not shown.
    #[test]
    fn an_empty_group_is_pruned() {
        let groups = vec![group(1, "prod", 0), group(2, "empty", 0)];
        let tree = build_asset_tree(&groups, &[asset(1, 1)]);
        assert_eq!(tree.len(), 1, "the empty group is dropped");
        assert_eq!(tree[0].group.name, "prod");
    }

    /// A parent cycle must not recurse without end.
    #[test]
    fn a_cycle_does_not_recurse_forever() {
        // Two groups that are each other's parent: unreachable from the root, so neither appears.
        let groups = vec![
            oxideterm_rayops::AssetGroup { id: 1, name: "a".into(), parent_id: 2, sort_order: 0, icon: String::new(), color: String::new() },
            oxideterm_rayops::AssetGroup { id: 2, name: "b".into(), parent_id: 1, sort_order: 0, icon: String::new(), color: String::new() },
        ];
        let tree = build_asset_tree(&groups, &[]);
        assert!(tree.is_empty(), "a cycle has no root, so the tree is empty rather than infinite");
}