// Everything needed to open a RayOps session, up to the point where a socket exists.
//
// Kept separate from the tab code because it is the only part that touches the network, and it
// is also the part that handles credentials: the password arrives from the environment, the JWT
// lives in this function, and the one-shot ticket is consumed by the handshake. Nothing here
// writes to disk, which is why the whole flow can be read in one screen.
//
// The flow is deliberately non-resumable. A RayOps reconnect is a new session — the server keeps
// no client-restorable shell — so there is no state to carry between attempts, and this function
// starts from a fresh session id every time.

/// Everything the flow needs, already read from settings.
pub(in crate::workspace) struct RayOpsLaunch {
    pub base_url: String,
    pub asset_id: i64,
    pub insecure_tls: bool,
    /// Whether the deployment may be plaintext. See `RayOpsSettings::allow_plaintext`.
    pub allow_plaintext: bool,
    pub username: String,
    /// Read from the environment rather than stored. See `credential_from_environment`.
    pub password: zeroize::Zeroizing<String>,
    pub token: Option<oxideterm_rayops::Secret>,
}

/// A connected terminal channel plus the identity this client presented for it.
pub(in crate::workspace) struct RayOpsConnection {
    pub socket: Box<dyn oxideterm_terminal::RayOpsSocket>,
    /// The `session_id` the ticket was bound to. Kept because it is the only join key back to
    /// the gateway's audit rows, and the UI shows it when a session is reported in the audit log.
    pub session_id: String,
}

impl std::fmt::Debug for RayOpsConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RayOpsConnection")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

/// Logs in, prechecks, mints a ticket and opens the socket.
///
/// Runs on the application's Tokio runtime. Every response goes through its `interpret_*`
/// function, and a response the client does not recognise is an error rather than something to
/// guess at: a gateway that changes shape must surface as a failure, not as a session that
/// silently never opens.
pub(in crate::workspace) async fn open_rayops_connection(
    launch: RayOpsLaunch,
) -> Result<RayOpsConnection, String> {
    use oxideterm_rayops::{
        LoginType, Secret, SessionId, TicketOutcome, Transport,
        access_precheck_request, assets_request, interpret_access_precheck, interpret_assets,
        interpret_login, interpret_ticket, login_request, ticket_request,
    };
    use oxideterm_rayops_net::ControlPlaneConfig;

    let mut config = ControlPlaneConfig::new(launch.base_url.clone());
    config.insecure_tls = launch.insecure_tls;
    config.allow_plaintext = launch.allow_plaintext;
    let control = oxideterm_rayops_net::ControlPlane::new(config).map_err(|e| e.to_string())?;

    // --- authenticate -----------------------------------------------------------------
    // An existing token skips login, which is how a session can be re-opened during development
    // without retyping a password.
    let session = match launch.token {
        Some(token) => oxideterm_rayops::Session {
            token,
            expires_at_raw: None,
            username: Some(launch.username.clone()),
            role: None,
        },
        None => match std::env::var("RAYOPS_JWT") {
            Ok(token) if !token.trim().is_empty() => oxideterm_rayops::Session {
                token: Secret::new(token.trim()),
                expires_at_raw: None,
                username: Some(launch.username.clone()),
                role: None,
            },
            _ => {
                let request = login_request(&launch.username, &Secret::new(launch.password.to_string()), LoginType::Local);
                let response = control.transport.send(&request).await.map_err(describe)?;
                interpret_login(&response).map_err(|e| e.to_string())?
            }
        },
    };

    // --- the asset must be visible to this user ---------------------------------------
    // Checked rather than assumed: `GetAssetForUser` filters by scope, and a ticket request for
    // an asset outside the user's scope answers 404, which reads as "does not exist" and hides
    // the real reason.
    let request = assets_request(&session.token, 1, 500);
    let response = control.transport.send(&request).await.map_err(describe)?;
    let page = interpret_assets(&response).map_err(|e| e.to_string())?;
    let asset = page
        .assets
        .iter()
        .find(|asset| asset.id == launch.asset_id)
        .ok_or_else(|| {
            format!(
                "asset {} is not visible to this user ({} assets on the first page)",
                launch.asset_id,
                page.assets.len()
            )
        })?;

    // --- the session identity ---------------------------------------------------------
    // Generated once and reused for the precheck, the ticket and the handshake. The gateway binds
    // the ticket to it and refuses a different one for 30 seconds, so a flow that regenerated it
    // between steps would block itself.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let session_id = SessionId::generate(asset.id, now_ms);

    // --- governance precheck ----------------------------------------------------------
    let request = access_precheck_request(&session.token, asset.id, session_id.as_str());
    let response = control.transport.send(&request).await.map_err(describe)?;
    match interpret_access_precheck(&response).map_err(|e| e.to_string())? {
        oxideterm_rayops::AccessDecision::Allowed => {}
        oxideterm_rayops::AccessDecision::Denied { reason } => {
            return Err(format!("governance denied this asset: {reason}"));
        }
        oxideterm_rayops::AccessDecision::ApprovalRequired {
            reason,
            approval_id,
        } => {
            return Err(match approval_id {
                Some(id) => format!("an approval is required before connecting ({reason}, approval {id})"),
                None => format!("an approval is required before connecting ({reason})"),
            });
        }
    }

    // --- one-shot ticket --------------------------------------------------------------
    let request = ticket_request(&session.token, asset.id, session_id.as_str());
    let response = control.transport.send(&request).await.map_err(describe)?;
    let grant = match interpret_ticket(&response).map_err(|e| e.to_string())? {
        TicketOutcome::Issued(grant) => grant,
        // `HTTP 200` without a ticket is not success: another session id holds the 30-second
        // start lease. Reported as its own error so the user learns to wait rather than retry.
        TicketOutcome::LeaseHeld {
            existing_session_id,
        } => {
            return Err(format!(
                "the start lease is held by {:?}; wait about 30 seconds, then try again",
                existing_session_id.unwrap_or_else(|| "<unknown>".to_owned())
            ));
        }
        TicketOutcome::UnknownSuccessShape { detail } => {
            return Err(format!(
                "the gateway answered 200 without a ticket and without a lease marker: {detail}"
            ));
        }
    };

    // --- handshake --------------------------------------------------------------------
    let url = control
        .config
        .terminal_url(asset.id, session_id.as_str(), grant.ticket.expose(), 80, 24)
        .map_err(|e| e)?;
    let socket_config = oxideterm_rayops_net::SocketConfig {
        insecure_tls: launch.insecure_tls,
        ..oxideterm_rayops_net::SocketConfig::default()
    };
    let socket = oxideterm_rayops_net::connect(&url, &socket_config)
        .await
        .map_err(|e| format!("the terminal handshake failed: {}", e.detail))?;

    // The ticket is consumed by the handshake above; `grant` goes out of scope with it.
    Ok(RayOpsConnection {
        socket: Box::new(socket),
        session_id: session_id.into_string(),
    })
}

/// Renders a control-plane failure without embedding anything sensitive.
fn describe(error: oxideterm_rayops::Error) -> String {
    error.to_string()
}

/// Builds the control plane for a launch request.
///
/// Extracted so the login, the asset list and the ticket exchange all construct it the same way —
/// a transport relaxation set on one path but not another would be a silent downgrade.
fn control_plane(launch: &RayOpsLaunch) -> Result<oxideterm_rayops_net::ControlPlane, String> {
    let mut config = oxideterm_rayops_net::ControlPlaneConfig::new(launch.base_url.clone());
    config.insecure_tls = launch.insecure_tls;
    config.allow_plaintext = launch.allow_plaintext;
    oxideterm_rayops_net::ControlPlane::new(config).map_err(|e| e.to_string())
}

/// Logs in and returns the session.
///
/// Split from the asset list so the modal can show the login result before asking for assets: a
/// failure here is about the deployment or the credentials, and mixing it with an empty asset list
/// would make a wrong password look like an empty inventory.
pub(in crate::workspace) async fn sign_in(
    launch: RayOpsLaunch,
) -> Result<oxideterm_rayops::Session, String> {
    use oxideterm_rayops::{
        LoginType, Secret, Transport, interpret_login, login_request,
    };

    let control = control_plane(&launch)?;
    let request = login_request(
        &launch.username,
        &Secret::new(launch.password.to_string()),
        LoginType::Local,
    );
    let response = control.transport.send(&request).await.map_err(describe)?;
    interpret_login(&response).map_err(|e| e.to_string())
}

/// One page of assets for a session.
pub(in crate::workspace) struct RayOpsAssetRequest {
    pub base_url: String,
    pub insecure_tls: bool,
    pub allow_plaintext: bool,
    pub token: oxideterm_rayops::Secret,
    /// Empty means "list everything", which is what the gateway does when the keyword is absent.
    pub keyword: String,
    pub page: i64,
    pub size: i64,
}

/// Fetches a page, using search when a keyword is present.
pub(in crate::workspace) async fn fetch_assets(
    request: RayOpsAssetRequest,
) -> Result<oxideterm_rayops::AssetPage, String> {
    use oxideterm_rayops::{
        Transport, asset_search_request, assets_request, interpret_assets,
    };

    let control = control_plane(&RayOpsLaunch {
        base_url: request.base_url,
        asset_id: 0,
        insecure_tls: request.insecure_tls,
        allow_plaintext: request.allow_plaintext,
        username: String::new(),
        password: zeroize::Zeroizing::new(String::new()),
        token: None,
    })?;
    let keyword = request.keyword.trim();
    let outbound = if keyword.is_empty() {
        assets_request(&request.token, request.page, request.size)
    } else {
        asset_search_request(&request.token, keyword, request.page, request.size)
    };
    let response = control.transport.send(&outbound).await.map_err(describe)?;
    interpret_assets(&response).map_err(|e| e.to_string())
}

/// The gateway's asset tree, so the picker can follow the operator's grouping.
///
/// Fetched alongside the first page rather than on demand: a group header with no name would be
/// worse than no grouping at all, and the tree is small — it is bounded by how many folders an
/// operator created, not by asset count.
pub(in crate::workspace) async fn fetch_asset_groups(
    base_url: String,
    insecure_tls: bool,
    allow_plaintext: bool,
    token: oxideterm_rayops::Secret,
) -> Result<Vec<oxideterm_rayops::AssetGroup>, String> {
    use oxideterm_rayops::{
        Transport, asset_groups_request, interpret_asset_groups,
    };

    let control = control_plane(&RayOpsLaunch {
        base_url,
        asset_id: 0,
        insecure_tls,
        allow_plaintext,
        username: String::new(),
        password: zeroize::Zeroizing::new(String::new()),
        token: None,
    })?;
    let request = asset_groups_request(&token);
    let response = control.transport.send(&request).await.map_err(describe)?;
    interpret_asset_groups(&response).map_err(|e| e.to_string())
}

/// Where a file operation runs, and the credential it presents.
///
/// The catalog's token is reused rather than signing in again: opening a directory must not
/// re-authenticate, and every extra login is another credential use for the gateway to audit.
pub(in crate::workspace) struct RayOpsFileRequest {
    pub base_url: String,
    pub insecure_tls: bool,
    pub allow_plaintext: bool,
    pub token: oxideterm_rayops::Secret,
    pub asset_id: i64,
}

impl RayOpsFileRequest {
    fn control_plane(&self) -> Result<oxideterm_rayops_net::ControlPlane, String> {
        let mut config = oxideterm_rayops_net::ControlPlaneConfig::new(self.base_url.clone());
        config.insecure_tls = self.insecure_tls;
        config.allow_plaintext = self.allow_plaintext;
        oxideterm_rayops_net::ControlPlane::new(config).map_err(|e| e.to_string())
    }
}

/// Lists one directory on a managed asset.
pub(in crate::workspace) async fn list_rayops_directory(
    request: RayOpsFileRequest,
    path: String,
) -> Result<oxideterm_rayops::SftpListing, String> {
    use oxideterm_rayops::{Transport, interpret_sftp_entries, sftp_browse_request};
    let control = request.control_plane()?;
    let built = sftp_browse_request(&request.token, request.asset_id, &path);
    let response = control.transport.send(&built).await.map_err(describe)?;
    interpret_sftp_entries(&response).map_err(describe)
}

/// Reads one file from a managed asset.
///
/// The whole file is returned rather than a stream: the gateway answers a download as a single
/// response, so there is nothing to stream from yet. A future channel-based transport is what would
/// change this, and it is why the signature returns owned bytes instead of a reader.
pub(in crate::workspace) async fn download_rayops_file(
    request: RayOpsFileRequest,
    path: String,
) -> Result<Vec<u8>, String> {
    use oxideterm_rayops::{Transport, interpret_sftp_download, sftp_download_request};
    let control = request.control_plane()?;
    let built = sftp_download_request(&request.token, request.asset_id, &path);
    let response = control.transport.send(&built).await.map_err(describe)?;
    interpret_sftp_download(&response).map_err(describe)
}

/// Sends a local file to a managed asset.
///
/// `local_path` is streamed by the transport; the bytes never pass through this function.
pub(in crate::workspace) async fn upload_rayops_file(
    request: RayOpsFileRequest,
    directory: String,
    relative_path: String,
    local_path: std::path::PathBuf,
) -> Result<(), String> {
    use oxideterm_rayops::{
        Endpoint, Transport, interpret_sftp_acknowledgement, sftp_upload_request,
    };
    let Some(file_name) = local_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return Err("the file to upload has no name".to_owned());
    };
    let control = request.control_plane()?;
    let built = sftp_upload_request(
        &request.token,
        request.asset_id,
        &directory,
        &relative_path,
        local_path,
        &file_name,
    );
    let response = control.transport.send(&built).await.map_err(describe)?;
    interpret_sftp_acknowledgement(&response, Endpoint::SftpUpload).map_err(describe)
}

/// Creates one directory on a managed asset.
pub(in crate::workspace) async fn create_rayops_directory(
    request: RayOpsFileRequest,
    path: String,
) -> Result<(), String> {
    use oxideterm_rayops::{Endpoint, Transport, interpret_sftp_acknowledgement, sftp_mkdir_request};
    let control = request.control_plane()?;
    let built = sftp_mkdir_request(&request.token, request.asset_id, &path);
    let response = control.transport.send(&built).await.map_err(describe)?;
    interpret_sftp_acknowledgement(&response, Endpoint::SftpMkdir).map_err(describe)
}

/// Deletes one entry on a managed asset.
pub(in crate::workspace) async fn delete_rayops_entry(
    request: RayOpsFileRequest,
    path: String,
) -> Result<(), String> {
    use oxideterm_rayops::{
        Endpoint, Transport, interpret_sftp_acknowledgement, sftp_delete_request,
    };
    let control = request.control_plane()?;
    let built = sftp_delete_request(&request.token, request.asset_id, &path);
    let response = control.transport.send(&built).await.map_err(describe)?;
    interpret_sftp_acknowledgement(&response, Endpoint::SftpDelete).map_err(describe)
}

/// Renames one entry on a managed asset.
pub(in crate::workspace) async fn rename_rayops_entry(
    request: RayOpsFileRequest,
    old_path: String,
    new_path: String,
) -> Result<(), String> {
    use oxideterm_rayops::{
        Endpoint, Transport, interpret_sftp_acknowledgement, sftp_rename_request,
    };
    let control = request.control_plane()?;
    let built = sftp_rename_request(&request.token, request.asset_id, &old_path, &new_path);
    let response = control.transport.send(&built).await.map_err(describe)?;
    interpret_sftp_acknowledgement(&response, Endpoint::SftpRename).map_err(describe)
}

/// Refuses an entry name that could place a write outside `parent`.
///
/// The name arrives from the gateway. Even for a gateway this client trusts, joining a
/// remote-supplied name onto a local path without checking it hands the *destination* of every
/// write to the remote side: `..`, `../x`, an absolute path, or a name containing a separator
/// would each escape the directory the user chose.
///
/// A name that is not a single ordinary path component is refused rather than normalised.
/// Rewriting it would silently accept something this client does not understand, and the user
/// would never learn that the listing was not what it appeared to be.
fn safe_local_child(parent: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    let mut components = std::path::Path::new(name).components();
    match (components.next(), components.next()) {
        // Exactly one `Normal` component, and nothing after it: this rejects "", ".", "..",
        // "/abs", "a/b", and "a/../b" without needing to enumerate those spellings.
        (Some(std::path::Component::Normal(_)), None) => Some(parent.join(name)),
        _ => None,
    }
}

/// What a recursive download brought back.
#[derive(Debug, Default, Eq, PartialEq)]
pub(in crate::workspace) struct RayOpsTreeDownload {
    pub files: usize,
    pub directories: usize,
    /// Paths that were listed but not fetched, with the reason. Reported rather than dropped:
    /// a partial download that looks complete is worse than one that says what it skipped.
    pub skipped: Vec<String>,
}

/// Downloads a directory and everything under it.
///
/// The gateway has no recursive or archive endpoint, so the walk happens here: list, descend,
/// fetch each file. It is many calls rather than one, which is why the count is reported — the cost
/// of this shape is the number of round trips, and the caller should be able to see it.
///
/// Entries this client cannot fetch as files — symlinks, devices, sockets — are collected into
/// `skipped` and **not** followed. Following a symlink is how a recursive walk ends up looping
/// forever or escaping the directory the user asked for.
pub(in crate::workspace) async fn download_rayops_tree(
    request: RayOpsFileRequest,
    remote_root: String,
    local_root: std::path::PathBuf,
) -> Result<RayOpsTreeDownload, String> {
    use oxideterm_rayops::{
        SftpEntryKind, Transport, interpret_sftp_download, interpret_sftp_entries,
        sftp_browse_request, sftp_download_request,
    };

    // One control plane for the whole walk: building a client per file would add a connection
    // setup to every entry in the tree.
    let control = request.control_plane()?;
    let mut summary = RayOpsTreeDownload::default();
    let mut queue = vec![(remote_root, local_root)];

    while let Some((remote_dir, local_dir)) = queue.pop() {
        std::fs::create_dir_all(&local_dir)
            .map_err(|error| format!("cannot create {}: {error}", local_dir.display()))?;
        summary.directories += 1;

        let listing = control
            .transport
            .send(&sftp_browse_request(&request.token, request.asset_id, &remote_dir))
            .await
            .map_err(describe)?;
        let listing = interpret_sftp_entries(&listing).map_err(describe)?;

        for entry in listing.entries {
            match entry.kind {
                SftpEntryKind::Directory => {
                    match safe_local_child(&local_dir, &entry.name) {
                        Some(child) => queue.push((entry.path.clone(), child)),
                        None => summary.skipped.push(entry.path.clone()),
                    }
                }
                SftpEntryKind::File => {
                    let response = control
                        .transport
                        .send(&sftp_download_request(
                            &request.token,
                            request.asset_id,
                            &entry.path,
                        ))
                        .await
                        .map_err(describe)?;
                    let bytes = interpret_sftp_download(&response).map_err(describe)?;
                    // Refused names are reported rather than written somewhere unexpected.
                    let Some(target) = safe_local_child(&local_dir, &entry.name) else {
                        summary.skipped.push(entry.path.clone());
                        continue;
                    };
                    std::fs::write(&target, &bytes)
                        .map_err(|error| format!("cannot write {}: {error}", target.display()))?;
                    summary.files += 1;
                }
                SftpEntryKind::Other => {
                    summary.skipped.push(entry.path.clone());
                }
            }
        }
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A remote-supplied name must never be able to place a write outside the chosen directory.
    ///
    /// Every case here is a way the gateway — or anything that can answer for it — could redirect
    /// the destination of a download. The assertion is on the resolved path, not merely on
    /// "some value came back": a check that returned `Some(root.join("../x"))` would pass a
    /// `is_some()` assertion and still write outside the root.
    #[test]
    fn a_remote_name_cannot_escape_the_download_directory() {
        let root = Path::new("/tmp/rayterm-download");

        for hostile in [
            "..",
            ".",
            "",
            "../escape.txt",
            "../../escape.txt",
            "sub/../../escape.txt",
            "/etc/passwd",
            "sub/file.txt",
            "./file.txt",
        ] {
            assert_eq!(
                safe_local_child(root, hostile),
                None,
                "{hostile:?} must be refused, not resolved"
            );
        }

        // An ordinary name is joined, and stays under the root.
        let joined = safe_local_child(root, "report.csv").expect("an ordinary name is accepted");
        assert_eq!(joined, root.join("report.csv"));
        assert!(joined.starts_with(root));

        // A dotfile is an ordinary component, not the `.` traversal.
        assert_eq!(
            safe_local_child(root, ".bashrc"),
            Some(root.join(".bashrc"))
        );
        // A name that merely contains dots is ordinary too.
        assert_eq!(
            safe_local_child(root, "archive.tar.gz"),
            Some(root.join("archive.tar.gz"))
        );
    }

    /// Component parsing follows the host platform, and that is the correct behaviour.
    ///
    /// On Unix a backslash is an ordinary filename character, so `a\b` is one component and is
    /// safe here; on Windows the same string is two components and is refused by the same check.
    /// Asserting a fixed answer for it would encode one platform's rule as the contract.
    #[test]
    fn separator_handling_follows_the_platform() {
        let root = Path::new("/tmp/rayterm-download");
        let resolved = safe_local_child(root, r"a\b");
        if cfg!(windows) {
            assert_eq!(resolved, None);
        } else {
            assert_eq!(resolved, Some(root.join(r"a\b")));
        }
    }
}
