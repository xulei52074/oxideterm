use crate::workspace::WorkspaceApp;
use crate::workspace::ime::WorkspaceImeTarget;
use oxideterm_connections::ConnectionTerminalOptions;
use oxideterm_gpui_ui::{TextInputView, text_input};
use oxideterm_theme::AppUiColors;
use gpui::{
    AnyElement, Context, MouseButton, ParentElement, Styled, Window, div, prelude::*, px, rgb,
    rgba,
};

// The RayOps login and asset-picking modal.
//
// Two steps rather than one form: credentials are checked before an asset can be chosen, because
// the asset list is scoped to the authenticated user. Doing it in one screen would mean showing a
// list that is empty until the user has typed a password, which reads as a broken deployment.
//
// The password is typed here and handed straight to the login request. It is never written to the
// settings file, and the token that comes back lives only in `RayOpsFlowState` for as long as the
// modal is open.

use super::rayops_state::{RayOpsBlock, RayOpsField, RayOpsFlowState, RayOpsPhase};

impl WorkspaceApp {
    /// Opens the RayOps modal, seeded from settings.
    pub(in crate::workspace) fn open_rayops_connection(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settings = self.settings_store.settings().rayops.clone();
        let state = RayOpsFlowState::from_settings(
            settings.base_url.trim().to_owned(),
            std::env::var("RAYOPS_USERNAME").unwrap_or_else(|_| "admin".to_owned()),
            settings.insecure_tls,
            settings.allow_plaintext,
        );
        self.connection_flow.update(cx, |flow, cx| {
            flow.rayops = Some(state);
            cx.notify();
        });
        let _ = window;
    }



    /// Builds the launch request from the current state.
    ///
    /// `consume_password` is false for the login call, which needs the password, and true for the
    /// later steps, which do not: the ticket exchange runs on the token, so the password is
    /// cleared as soon as it has been used once.
    fn rayops_launch_from_state(
        &mut self,
        cx: &mut Context<Self>,
        consume_password: bool,
    ) -> Option<super::super::rayops_flow::RayOpsLaunch> {
        self.connection_flow.update(cx, |flow, _| {
            let state = flow.rayops.as_mut()?;
            // The password is read from the environment rather than typed into the dialog: the
            // text fields in this modal are not wired to the input handler yet, and a password
            // box that cannot be typed into would be worse than an explicit external source.
            // Typed entry belongs with that wiring, in the same change.
            let password = if state.password.is_empty() {
                super::super::rayops_flow::credential_from_environment()
                    .map(|password| {
                        state.password = password.clone();
                        password
                    })
                    .unwrap_or_else(|_| zeroize::Zeroizing::new(String::new()))
            } else if consume_password {
                std::mem::take(&mut state.password)
            } else {
                state.password.clone()
            };
            Some(super::super::rayops_flow::RayOpsLaunch {
                base_url: state.base_url.trim().to_owned(),
                asset_id: 0,
                insecure_tls: state.insecure_tls,
                allow_plaintext: state.allow_plaintext,
                username: state.username.clone(),
                password,
            })
        })
    }

    /// Builds an asset-page request from the current state.
    fn rayops_asset_request_from_state(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<super::super::rayops_flow::RayOpsAssetRequest> {
        let state = self.connection_flow.read(cx).rayops.as_ref()?;
        Some(super::super::rayops_flow::RayOpsAssetRequest {
            base_url: state.base_url.trim().to_owned(),
            insecure_tls: state.insecure_tls,
            allow_plaintext: state.allow_plaintext,
            token: state.token.clone()?,
            keyword: state.search.clone(),
            page: state.page.index as i64,
            size: state.page.size as i64,
        })
    }

    /// Applies a login result, ignoring one that has been superseded.
    fn apply_rayops_login_outcome(
        &mut self,
        generation: u64,
        outcome: Result<Result<oxideterm_rayops::Session, String>, tokio::task::JoinError>,
        cx: &mut Context<Self>,
    ) {
        // Destructured once: matching on the outcome twice would move it.
        let (session, block) = match outcome {
            Ok(Ok(session)) => (Some(session), None),
            Ok(Err(message)) => (None, Some(RayOpsBlock::Failed { message })),
            Err(join) => (
                None,
                Some(RayOpsBlock::Failed {
                    message: format!("the login task failed: {join}"),
                }),
            ),
        };
        let logged_in = session.is_some();
        self.connection_flow.update(cx, |flow, cx| {
            let Some(state) = flow.rayops.as_mut() else {
                return;
            };
            if !state.is_current(generation) {
                return;
            }
            state.block = block;
            match session {
                Some(session) => {
                    state.token = Some(session.token);
                    state.phase = RayOpsPhase::Browsing { loading: true };
                }
                None => {
                    state.phase = RayOpsPhase::Credentials;
                }
            }
            cx.notify();
        });
        if logged_in {
            self.reload_rayops_assets(cx);
        }
    }

    /// Applies an asset-page result, ignoring one that has been superseded.
    fn apply_rayops_assets_outcome(
        &mut self,
        generation: u64,
        outcome: Result<Result<oxideterm_rayops::AssetPage, String>, tokio::task::JoinError>,
        cx: &mut Context<Self>,
    ) {
        self.connection_flow.update(cx, |flow, cx| {
            let Some(state) = flow.rayops.as_mut() else {
                return;
            };
            if !state.is_current(generation) {
                return;
            }
            match outcome {
                Ok(Ok(page)) => {
                    // More pages are inferred from the total the gateway reports rather than
                    // from a short page, because a deployment may return fewer than requested
                    // on a page that is not the last.
                    let seen = page.page.saturating_mul(page.size);
                    state.has_more = seen < page.total;
                    state.assets = page.assets;
                    state.block = None;
                }
                Ok(Err(message)) => {
                    state.assets.clear();
                    state.has_more = false;
                    state.block = Some(RayOpsBlock::Failed { message });
                }
                Err(join) => {
                    state.block = Some(RayOpsBlock::Failed {
                        message: format!("the asset request failed: {join}"),
                    });
                }
            }
            state.phase = RayOpsPhase::Browsing { loading: false };
            cx.notify();
        });
    }

    /// Applies the outcome of the ticket exchange, opening a tab on success.
    fn apply_rayops_connect_outcome(
        &mut self,
        generation: u64,
        title: String,
        terminal_options: ConnectionTerminalOptions,
        outcome: Result<Result<super::super::rayops_flow::RayOpsConnection, String>, tokio::task::JoinError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = match outcome {
            Ok(Ok(connection)) => Ok(connection),
            Ok(Err(message)) => Err(RayOpsBlock::classify(message)),
            Err(join) => Err(RayOpsBlock::Failed {
                message: format!("the connection task failed: {join}"),
            }),
        };
        match result {
            Ok(connection) => {
                // The flow ends here: the session owns the socket now, and the token has no
                // further use, so it is dropped with the state.
                self.close_rayops_flow(cx);
                if let Err(error) = self.create_rayops_terminal_tab(
                    title,
                    connection.socket,
                    terminal_options,
                    window,
                    cx,
                ) {
                    self.notify_rayops_error(error.to_string(), cx);
                }
            }
            Err(block) => {
                self.connection_flow.update(cx, |flow, cx| {
                    let Some(state) = flow.rayops.as_mut() else {
                        return;
                    };
                    if !state.is_current(generation) {
                        return;
                    }
                    state.block = Some(block);
                    cx.notify();
                });
            }
        }
    }

    /// Closes the flow, dropping the token and the password.
    pub(in crate::workspace) fn close_rayops_flow(&mut self, cx: &mut Context<Self>) {
        self.connection_flow.update(cx, |flow, cx| {
            if let Some(state) = flow.rayops.as_mut() {
                // A cancelled attempt must not leave a live token or a typed password in memory
                // until the entity happens to be dropped.
                state.forget_credentials();
            }
            flow.rayops = None;
            cx.notify();
        });
    }

    /// Logs in, then loads the first page of assets.
    pub(in crate::workspace) fn begin_rayops_login(&mut self, cx: &mut Context<Self>) {
        let Some(launch) = self.rayops_launch_from_state(cx, false) else {
            return;
        };
        let runtime = self.forwarding_runtime.clone();
        let generation = self.connection_flow.update(cx, |flow, _| {
            let state = flow.rayops.as_mut()?;
            state.block = None;
            state.phase = RayOpsPhase::Authenticating;
            Some(state.next_generation())
        });
        let Some(generation) = generation else { return };

        let task = cx.spawn(async move |this, cx| {
            let outcome = runtime
                .spawn(super::super::rayops_flow::sign_in(launch))
                .await;
            let _ = this.update_in(cx, |this, _window, cx| {
                this.apply_rayops_login_outcome(generation, outcome, cx);
            });
        });
        task.detach();
    }

    /// Reloads the asset list for the current search text and page.
    pub(in crate::workspace) fn reload_rayops_assets(&mut self, cx: &mut Context<Self>) {
        let Some(request) = self.rayops_asset_request_from_state(cx) else {
            return;
        };
        let runtime = self.forwarding_runtime.clone();
        let generation = self.connection_flow.update(cx, |flow, _| {
            let state = flow.rayops.as_mut()?;
            state.block = None;
            state.phase = RayOpsPhase::Browsing { loading: true };
            Some(state.next_generation())
        });
        let Some(generation) = generation else { return };

        let task = cx.spawn(async move |this, cx| {
            let outcome = runtime
                .spawn(super::super::rayops_flow::fetch_assets(request))
                .await;
            let _ = this.update_in(cx, |this, _window, cx| {
                this.apply_rayops_assets_outcome(generation, outcome, cx);
            });
        });
        task.detach();
    }

    pub(in crate::workspace) fn next_rayops_page(&mut self, cx: &mut Context<Self>) {
        self.connection_flow.update(cx, |flow, _| {
            if let Some(state) = flow.rayops.as_mut() {
                state.page.index += 1;
            }
        });
        self.reload_rayops_assets(cx);
    }

    pub(in crate::workspace) fn previous_rayops_page(&mut self, cx: &mut Context<Self>) {
        self.connection_flow.update(cx, |flow, _| {
            if let Some(state) = flow.rayops.as_mut() {
                state.page.index = state.page.index.saturating_sub(1).max(1);
            }
        });
        self.reload_rayops_assets(cx);
    }

    /// Runs the governance precheck, mints a ticket and opens the session.
    pub(in crate::workspace) fn connect_rayops_asset(
        &mut self,
        asset_id: i64,
        cx: &mut Context<Self>,
    ) {
        let Some(mut launch) = self.rayops_launch_from_state(cx, true) else {
            return;
        };
        launch.asset_id = asset_id;
        let runtime = self.forwarding_runtime.clone();
        let title = self
            .connection_flow
            .read(cx)
            .rayops
            .as_ref()
            .and_then(|state| {
                state
                    .assets
                    .iter()
                    .find(|asset| asset.id == asset_id)
                    .map(|asset| asset.hostname.clone())
            })
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| format!("RayOps {asset_id}"));
        let generation = self.connection_flow.update(cx, |flow, _| {
            let state = flow.rayops.as_mut()?;
            state.block = None;
            Some(state.next_generation())
        });
        let Some(generation) = generation else { return };
        let terminal_options = ConnectionTerminalOptions::default();

        let task = cx.spawn(async move |this, cx| {
            let outcome = runtime
                .spawn(super::super::rayops_flow::open_rayops_connection(launch))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.apply_rayops_connect_outcome(
                    generation,
                    title,
                    terminal_options,
                    outcome,
                    window,
                    cx,
                );
            });
        });
        task.detach();
    }

    /// Renders the modal, or an empty element when no flow is open.
    pub(in crate::workspace) fn render_rayops_connection_modal(
        &self,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(state) = self.connection_flow.read(cx).rayops.clone() else {
            return div().into_any_element();
        };
        let theme = self.tokens.ui;
        let mut card = div()
            .flex()
            .flex_col()
            .gap_3()
            .w(px(560.0))
            .p_5()
            .rounded_lg()
            .bg(rgb(theme.bg_card))
            .border_1()
            .border_color(rgb(theme.border))
            .child(
                div()
                    .text_lg()
                    .text_color(rgb(theme.text))
                    .child(self.i18n.t("command_palette.cmd_new_rayops_connection")),
            );

        match &state.phase {
            RayOpsPhase::Credentials | RayOpsPhase::Authenticating => {
                card = self.render_rayops_credentials_step(card, &state, theme, cx);
            }
            RayOpsPhase::Browsing { .. } => {
                card = self.render_rayops_asset_step(card, &state, theme, cx);
            }
        }

        if let Some(block) = state.block.as_ref() {
            card = card.child(
                div()
                    .p_3()
                    .rounded_md()
                    .bg(rgb(theme.bg_sunken))
                    .text_color(rgb(theme.error))
                    .child(block.message()),
            );
        }

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000088))
            .child(card)
            .into_any_element()
    }

    /// The credentials step: deployment, username, password.
    fn render_rayops_credentials_step(
        &self,
        mut card: gpui::Div,
        state: &RayOpsFlowState,
        theme: AppUiColors,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let busy = matches!(state.phase, RayOpsPhase::Authenticating);
        card = card.child(self.render_rayops_labelled_input(
            "ssh.form.host",
            &state.base_url,
            "https://rayops.example",
            RayOpsField::BaseUrl,
            theme,
            cx,
        ));
        card = card.child(self.render_rayops_labelled_input(
            "ssh.form.username",
            &state.username,
            "admin",
            RayOpsField::Username,
            theme,
            cx,
        ));
        card = card.child(self.render_rayops_labelled_input(
            "ssh.form.password",
            state.password.as_str(),
            "",
            RayOpsField::Password,
            theme,
            cx,
        ));

        // Both transport relaxations are stated in the dialog rather than only in settings: the
        // user is about to type a password, and should know whether it travels encrypted.
        if state.base_url.starts_with("http://") || state.insecure_tls {
            let mut warning = String::new();
            if state.base_url.starts_with("http://") {
                warning.push_str("This deployment is plain HTTP: the password, the session token and the connection ticket are sent unencrypted.\n");
            }
            if state.insecure_tls {
                warning.push_str("Certificate verification is disabled, so any server can present itself as this deployment.\n");
            }
            card = card.child(
                div()
                    .p_3()
                    .rounded_md()
                    .bg(rgb(theme.bg_sunken))
                    .text_color(rgb(theme.warning))
                    .child(warning.trim_end().to_owned()),
            );
        }

        card.child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .justify_end()
                .child(self.render_rayops_button("ssh.form.cancel".to_owned(), false, false, theme, cx, |this, cx| {
                    this.close_rayops_flow(cx);
                }))
                .child(self.render_rayops_button(
                    if busy { "ssh.form.connecting" } else { "ssh.form.connect" }.to_owned(),
                    true,
                    busy,
                    theme,
                    cx,
                    |this, cx| this.begin_rayops_login(cx),
                )),
        )
    }

    /// The asset step: search, page through, choose one.
    fn render_rayops_asset_step(
        &self,
        mut card: gpui::Div,
        state: &RayOpsFlowState,
        theme: AppUiColors,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        card = card.child(self.render_rayops_labelled_input(
            "ssh.list.search_placeholder",
            &state.search,
            "",
            RayOpsField::AssetSearch,
            theme,
            cx,
        ));
        card = card.child(self.render_rayops_button(
            "ssh.list.search".to_owned(),
            false,
            matches!(state.phase, RayOpsPhase::Browsing { loading: true }),
            theme,
            cx,
            |this, cx| this.reload_rayops_assets(cx),
        ));

        if state.assets.is_empty() {
            card = card.child(div().text_color(rgb(theme.text_muted)).child(
                self.i18n.t("ssh.list.empty"),
            ));
        } else {
            let mut list = div().flex().flex_col().gap_1().max_h(px(280.0)).overflow_hidden();
            for asset in &state.assets {
                let id = asset.id;
                // `Asset` carries no display name; the hostname is what identifies it to the
                // operator, with the id kept alongside because two assets can share a hostname.
                let label = if asset.hostname.trim().is_empty() {
                    format!("#{id}")
                } else {
                    format!("{} ({}) (#{id})", asset.hostname, asset.ip)
                };
                list = list.child(
                    div()
                        .id(("rayops-asset", id as u64))
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(theme.bg_elevated))
                        .text_color(rgb(theme.text))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(theme.bg_hover)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &gpui::MouseDownEvent, _window, cx| {
                                this.connect_rayops_asset(id, cx);
                            }),
                        )
                        .child(label),
                );
            }
            card = card.child(list);
        }

        card = card
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .justify_between()
                    .child(self.render_rayops_button(
                        "ssh.form.cancel".to_owned(),
                        false,
                        false,
                        theme,
                        cx,
                        |this, cx| this.close_rayops_flow(cx),
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(self.render_rayops_button(
                                "ssh.list.previous_page".to_owned(),
                                false,
                                state.page.index <= 1,
                                theme,
                                cx,
                                |this, cx| this.previous_rayops_page(cx),
                            ))
                            .child(self.render_rayops_button(
                                "ssh.list.next_page".to_owned(),
                                false,
                                !state.has_more,
                                theme,
                                cx,
                                |this, cx| this.next_rayops_page(cx),
                            )),
                    ),
            );

        card
    }

    /// A labelled text field.
    ///
    /// `apply` writes the edited value back into the flow state, so each field declares its own
    /// target rather than the caller matching on an enum.
    #[allow(clippy::too_many_arguments)]
    fn render_rayops_labelled_input(
        &self,
        label_key: &str,
        value: &str,
        placeholder: &str,
        field: RayOpsField,
        theme: AppUiColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focused = self
            .connection_flow
            .read(cx)
            .rayops
            .as_ref()
            .is_some_and(|state| state.focused_field == Some(field));
        let input = text_input(
            &self.tokens,
            TextInputView {
                value,
                placeholder: placeholder.to_owned(),
                focused,
                caret_visible: focused && self.input_caret.visible(),
                secret: field.is_secret(),
                selected_all: false,
                selected_range: None,
                marked_text: None,
            },
        )
        .id(("rayops-field", field.index()));

        let target = WorkspaceImeTarget::RayOps(field);
        let probe = self.text_input_with_workspace_ime(
            target,
            input,
            move |this, cx| {
                this.connection_flow.update(cx, |flow, cx| {
                    if let Some(state) = flow.rayops.as_mut() {
                        state.focused_field = Some(field);
                        cx.notify();
                    }
                });
                this.show_active_input_caret(cx);
            },
            cx,
        );

        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(theme.text_muted))
                    .child(self.i18n.t(label_key)),
            )
            .child(probe)
            .into_any_element()
    }

    /// A flat button that runs an action on the workspace.
    fn render_rayops_button(
        &self,
        label: String,
        primary: bool,
        disabled: bool,
        theme: AppUiColors,
        cx: &mut Context<Self>,
        action: fn(&mut Self, &mut Context<Self>),
    ) -> AnyElement {
        let label = if label.contains('.') {
            self.i18n.t(&label)
        } else {
            label
        };
        let mut button = div()
            .px_3()
            .py_2()
            .rounded_md()
            .text_color(rgb(if disabled { theme.text_muted } else { theme.text }))
            .bg(rgb(if primary { theme.accent } else { theme.bg_elevated }));
        if !disabled {
            button = button.cursor_pointer().on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &gpui::MouseDownEvent, _window, cx| action(this, cx)),
            );
        }
        button.child(label).into_any_element()
    }
}
