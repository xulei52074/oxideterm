// Copyright (C) 2026 AnalyseDeCircuit
// SPDX-License-Identifier: GPL-3.0-only

//! RayTerm's integration layer inside the oxideTerm workspace.
//!
//! # Ownership
//!
//! This crate is the boundary between two things that must not be confused:
//!
//! * the **RayTerm repository** (`../../../crates/`), which holds the RayOps protocol client
//!   — frame codec, control plane, session state machine, reconnect policy. That code
//!   belongs to RayTerm, is buildable without the GPUI tree, and is verified against a real
//!   deployment independently of this fork.
//! * the **oxideTerm fork**, which owns everything about rendering, panes, tabs and the
//!   session lifecycle. This crate is the only place where the two meet.
//!
//! Keeping the protocol client out of the workspace means an upstream merge never has to
//! reconcile files that were never upstream's, and it keeps the client testable on a machine
//! that cannot compile the GPUI layer.
//!
//! # What belongs here
//!
//! Only the adaptation: anything that has to name both a RayOps concept and an oxideTerm
//! concept. A change that is purely about the wire protocol belongs in the protocol crates;
//! a change that is purely about rendering belongs in the GPUI crates.
//!
//! See `docs/adr/0002-session-integration.md` for the decision that fixes this crate's
//! scope: RayOps sessions reuse and lightly generalise oxideTerm's existing terminal-endpoint
//! ownership rather than becoming a first-class session type.

/// The RayOps protocol client, re-exported so callers inside the workspace need only one
/// dependency to reach the whole client surface.
pub use oxideterm_rayops as protocol;

/// The network wiring for the protocol client: the `reqwest` control plane and the
/// `tokio-tungstenite` terminal socket.
pub use oxideterm_rayops_net as net;

/// How a RayOps deployment is addressed.
///
/// Mirrors [`net::ControlPlaneConfig`] rather than aliasing it, so a later addition that is
/// specific to the application — a saved deployment list, a per-workspace override — has a
/// place to live without changing the protocol crate's public shape.
#[derive(Debug, Clone)]
pub struct Deployment {
    /// Deployment origin, for example `http://192.168.240.50`. The terminal endpoint shares
    /// this origin, so there is no second URL to configure.
    pub base_url: String,
    /// Accept any TLS certificate. Off by default and named so that every enabling site is
    /// explicit; see the protocol client's `tls` module for what it costs.
    pub insecure_tls: bool,
}

impl Deployment {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            insecure_tls: false,
        }
    }

    /// The control-plane configuration this deployment implies.
    pub fn control_plane_config(&self) -> net::ControlPlaneConfig {
        let mut config = net::ControlPlaneConfig::new(self.base_url.clone());
        config.insecure_tls = self.insecure_tls;
        config
    }

    /// The socket configuration this deployment implies.
    pub fn socket_config(&self) -> net::SocketConfig {
        net::SocketConfig {
            insecure_tls: self.insecure_tls,
            ..net::SocketConfig::default()
        }
    }

    /// Whether the deployment is addressed over TLS.
    ///
    /// Exposed because the answer is worth surfacing in the UI: a terminal session and its
    /// ticket travel in the clear on a plain `http://` origin, and an operator should be
    /// able to see that rather than infer it from the URL string.
    pub fn uses_tls(&self) -> bool {
        self.base_url.starts_with("https://")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deployment_defaults_to_verifying_certificates() {
        // The default matters: an operator who types a bare URL must get verification, and
        // turning it off has to be a deliberate act at a named site.
        let deployment = Deployment::new("https://rayops.internal");
        assert!(!deployment.insecure_tls);
        assert!(!deployment.control_plane_config().insecure_tls);
        assert!(!deployment.socket_config().insecure_tls);
        assert!(deployment.uses_tls());
    }

    #[test]
    fn disabling_verification_applies_to_both_channels() {
        // The control plane and the terminal socket are separate HTTP/WebSocket clients, so
        // a setting that reached only one of them would leave the other failing against a
        // self-signed gateway — the failure mode this exists to avoid.
        let mut deployment = Deployment::new("https://rayops.internal");
        deployment.insecure_tls = true;
        assert!(deployment.control_plane_config().insecure_tls);
        assert!(deployment.socket_config().insecure_tls);
    }

    #[test]
    fn a_plain_http_deployment_is_reported_as_not_using_tls() {
        // Worth surfacing: on a plain origin the ticket and the JWT travel in the clear.
        let deployment = Deployment::new("http://192.168.240.50");
        assert!(!deployment.uses_tls());
        assert!(
            !deployment.insecure_tls,
            "an unencrypted origin and an unverified certificate are different settings"
        );
    }

    #[test]
    fn a_trailing_slash_survives_into_the_control_plane_configuration() {
        // The protocol client normalises it; this only pins that the wrapper does not
        // mangle the value on the way through.
        let deployment = Deployment::new("https://rayops.internal/");
        assert_eq!(
            deployment.control_plane_config().base_url,
            "https://rayops.internal/"
        );
    }
}
