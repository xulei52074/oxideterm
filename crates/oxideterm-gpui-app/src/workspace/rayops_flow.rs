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

/// Reads the RayOps password from the environment.
///
/// A deliberate stand-in until the login form exists: a password must not be written to the
/// settings file, and the alternative — a placeholder that does nothing — would make the entry
/// point untestable end to end. The variable name matches the verification probe in the RayTerm
/// repository so both use one documented source.
pub(in crate::workspace) fn credential_from_environment() -> Result<zeroize::Zeroizing<String>, String> {
    match std::env::var("RAYOPS_PASSWORD") {
        Ok(value) if !value.is_empty() => Ok(zeroize::Zeroizing::new(value)),
        Ok(_) => Err("RAYOPS_PASSWORD is set but empty".to_owned()),
        Err(_) => Err(
            "RAYOPS_PASSWORD is not set. The password is read from the environment because \
             storing it would put a server credential in a settings file that is exported and \
             synced."
                .to_owned(),
        ),
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
    let session = match std::env::var("RAYOPS_JWT") {
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
    };

    // --- the asset must be visible to this user ---------------------------------------
    // Checked rather than assumed: `GetAssetForUser` filters by scope, and a ticket request for
    // an asset outside the user's scope answers 404, which reads as "does not exist" and hides
    // the real reason.
    let request = assets_request(&session.token, 1, 200);
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
