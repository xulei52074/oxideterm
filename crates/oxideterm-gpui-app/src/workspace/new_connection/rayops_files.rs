//! Browsing and transferring files on a managed RayOps asset.
//!
//! This is deliberately separate from the saved-connection SFTP surface. That one speaks the SFTP
//! protocol over a `russh` channel it owns; this one asks the RayOps gateway to read and write the
//! asset on the client's behalf, and the client never holds a credential or opens a second SSH
//! path. The two cannot share a transport, so they do not pretend to.

use gpui::Context;

/// The path split into clickable ancestors, outermost first.
///
/// Each entry pairs the label to show with the absolute path it navigates to, so a segment can be
/// rendered and acted on without the view re-deriving what a prefix means. The last entry is the
/// directory on display.
///
/// The root is always present, which is what makes the whole path reachable from any depth without
/// stepping up one level at a time.
pub(in crate::workspace) fn path_segments(path: &str) -> Vec<(String, String)> {
    let trimmed = path.trim_matches('/');
    let mut segments = vec![("/".to_owned(), "/".to_owned())];
    if trimmed.is_empty() {
        return segments;
    }
    let mut accumulated = String::new();
    for part in trimmed.split('/').filter(|part| !part.is_empty()) {
        accumulated.push('/');
        accumulated.push_str(part);
        segments.push((part.to_owned(), accumulated.clone()));
    }
    segments
}

/// Whether an asset advertises the capability the gateway's file API requires.
///
/// Checked before the user is offered a file view, so an asset that cannot serve one is not
/// presented with an entry that would only fail. The gateway answers `400 asset does not support
/// sftp` for these, and reaching that error is a worse experience than never offering it.
///
/// The check is on the gateway's own `protocols` list rather than on the platform: which assets
/// serve files is the gateway's statement about the asset, not something this client infers.
pub(in crate::workspace) fn asset_supports_files(asset: &oxideterm_rayops::Asset) -> bool {
    asset
        .protocols
        .iter()
        .any(|protocol| protocol.eq_ignore_ascii_case("sftp"))
}

/// The path of `name` inside `parent`.
///
/// A trailing separator on `parent` is not doubled, because the gateway echoes paths back and a
/// doubled separator would make two spellings of the same directory look like different ones.
pub(in crate::workspace) fn child_path(parent: &str, name: &str) -> String {
    let parent = parent.trim_end_matches('/');
    if parent.is_empty() {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// The parent of `path`, or `None` at the root.
///
/// `None` rather than `"/"` so a caller can disable "go up" at the root instead of re-listing the
/// same directory and appearing to do nothing.
pub(in crate::workspace) fn parent_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        // The root itself.
        return None;
    }
    match trimmed.rfind('/') {
        // A top-level entry: `/etc` has the root as its parent.
        Some(0) => Some("/".to_owned()),
        Some(index) => Some(trimmed[..index].to_owned()),
        // A relative path should not reach here, since every path this view builds is absolute.
        None => None,
    }
}

/// What the file view is showing.
#[derive(Debug, Default)]
pub(in crate::workspace) struct RayOpsFileState {
    /// The asset being browsed, or `None` when the view is closed.
    pub asset_id: Option<i64>,
    /// The directory on display. Absolute, as the gateway reports paths.
    pub path: String,
    pub entries: Vec<oxideterm_rayops::SftpEntry>,
    pub phase: RayOpsFilePhase,
    /// The gateway's own message for the last failure, shown as it arrived.
    pub error: Option<String>,
    /// Index into `entries`, not a name: two entries cannot share a path, and a name can be
    /// re-used by a refresh between the click and the action.
    pub selected: Option<usize>,
}

impl RayOpsFileState {
    /// Opens the view on an asset's root.
    pub(in crate::workspace) fn open(asset_id: i64) -> Self {
        Self {
            asset_id: Some(asset_id),
            path: "/".to_owned(),
            entries: Vec::new(),
            phase: RayOpsFilePhase::Loading,
            error: None,
            selected: None,
        }
    }

    /// Whether the view has an asset to browse.
    pub(in crate::workspace) fn is_open(&self) -> bool {
        self.asset_id.is_some()
    }

    /// The selected entry, if the selection still addresses one.
    ///
    /// Returns `None` after a refresh that shortened the list, rather than indexing out of bounds.
    pub(in crate::workspace) fn selected_entry(&self) -> Option<&oxideterm_rayops::SftpEntry> {
        self.selected.and_then(|index| self.entries.get(index))
    }
}

/// Where the file view is in its load cycle.
#[derive(Debug, Default, Eq, PartialEq)]
pub(in crate::workspace) enum RayOpsFilePhase {
    /// Never opened, or closed again.
    #[default]
    Closed,
    Loading,
    Ready,
    /// The gateway refused or the call failed; `error` carries its message.
    Failed,
}

#[cfg(test)]
mod rayops_files_tests {
    use super::*;

    fn asset_with_protocols(protocols: &[&str]) -> oxideterm_rayops::Asset {
        oxideterm_rayops::Asset {
            id: 1,
            hostname: "h".to_owned(),
            ip: "10.0.0.1".to_owned(),
            port: 22,
            platform: "linux".to_owned(),
            protocols: protocols.iter().map(|p| (*p).to_owned()).collect(),
            os: String::new(),
            group_id: 0,
            tags: Vec::new(),
            credential_verify_status: String::new(),
            credential_id: None,
            credential_name: None,
            credential_source: None,
            credential_status: None,
            credential_verify_message: None,
            credential_verified_at: None,
            username: None,
            auth_type: None,
            has_password: false,
            has_private_key: false,
            has_passphrase: false,
            sudo_mode: None,
            has_sudo_password: false,
            cpu: None,
            memory: None,
            disk: None,
            status: None,
        }
    }

    /// The file view follows the gateway's `protocols`, not the platform.
    ///
    /// Two of the estate's assets are Windows and advertise only `rdp`/`winrm`/`smb`; the gateway
    /// refuses them with `400 asset does not support sftp`. Checking here means the user is never
    /// offered an entry that cannot work.
    #[test]
    fn file_support_follows_the_gateways_protocol_list() {
        assert!(asset_supports_files(&asset_with_protocols(&["ssh", "sftp"])));
        assert!(asset_supports_files(&asset_with_protocols(&["SFTP"])));
        assert!(!asset_supports_files(&asset_with_protocols(&[
            "rdp", "winrm", "smb"
        ])));
        assert!(!asset_supports_files(&asset_with_protocols(&[])));
    }

    /// Child paths join without doubling or dropping the separator.
    #[test]
    fn a_child_path_joins_the_parent_and_the_name() {
        assert_eq!(child_path("/", "etc"), "/etc");
        assert_eq!(child_path("/etc", "nginx"), "/etc/nginx");
        // The gateway echoes paths, so a doubled separator would spell one directory two ways.
        assert_eq!(child_path("/etc/", "nginx"), "/etc/nginx");
    }

    /// Going up stops at the root rather than re-listing it.
    #[test]
    fn a_parent_path_stops_at_the_root() {
        assert_eq!(parent_path("/etc"), Some("/".to_owned()));
        assert_eq!(parent_path("/etc/nginx"), Some("/etc".to_owned()));
        assert_eq!(parent_path("/etc/nginx/"), Some("/etc".to_owned()));
        assert_eq!(
            parent_path("/"),
            None,
            "the root has no parent, so 'go up' can be disabled rather than silently reloading"
        );
        assert_eq!(parent_path(""), None);
    }

    /// A refresh that shortens the list must not leave a selection pointing past the end.
    #[test]
    fn a_selection_that_no_longer_addresses_an_entry_reads_as_none() {
        let mut state = RayOpsFileState::open(7);
        assert_eq!(state.asset_id, Some(7));
        assert_eq!(state.path, "/");
        assert_eq!(state.phase, RayOpsFilePhase::Loading);
        assert!(state.is_open());

        state.selected = Some(3);
        assert!(state.selected_entry().is_none(), "an empty list has no entry 3");
    }

    /// Every ancestor is reachable in one step, and the deepest entry is the current directory.
    #[test]
    fn the_path_splits_into_clickable_ancestors() {
        assert_eq!(path_segments("/"), vec![("/".to_owned(), "/".to_owned())]);
        assert_eq!(
            path_segments("/data"),
            vec![
                ("/".to_owned(), "/".to_owned()),
                ("data".to_owned(), "/data".to_owned()),
            ]
        );
        assert_eq!(
            path_segments("/data/sit-backup/mongodb"),
            vec![
                ("/".to_owned(), "/".to_owned()),
                ("data".to_owned(), "/data".to_owned()),
                ("sit-backup".to_owned(), "/data/sit-backup".to_owned()),
                ("mongodb".to_owned(), "/data/sit-backup/mongodb".to_owned()),
            ]
        );
        // A trailing separator names the same directory, not an extra empty segment.
        assert_eq!(
            path_segments("/data/"),
            vec![
                ("/".to_owned(), "/".to_owned()),
                ("data".to_owned(), "/data".to_owned()),
            ]
        );
        // An empty path still offers the root rather than nothing to click.
        assert_eq!(path_segments(""), vec![("/".to_owned(), "/".to_owned())]);
    }
}

impl crate::workspace::WorkspaceApp {
    /// Builds the credential-bearing request the file calls need, or `None` when the catalog has
    /// no session to borrow.
    fn rayops_file_request(&self, asset_id: i64) -> Option<super::super::rayops_flow::RayOpsFileRequest> {
        let token = self.rayops_catalog.token()?;
        let base_url = self.rayops_catalog.base_url.clone();
        if base_url.is_empty() {
            return None;
        }
        let settings = self.settings_store.settings().rayops.clone();
        Some(super::super::rayops_flow::RayOpsFileRequest {
            base_url,
            insecure_tls: settings.insecure_tls,
            allow_plaintext: settings.allow_plaintext,
            token,
            asset_id,
        })
    }

    /// Opens the file view on an asset and loads its root.
    ///
    /// Refuses an asset the gateway does not advertise as serving files, rather than opening a view
    /// whose every action would fail with `asset does not support sftp`.
    pub(in crate::workspace) fn open_rayops_files(&mut self, asset_id: i64, cx: &mut Context<Self>) {
        let supported = self
            .rayops_catalog
            .assets
            .iter()
            .find(|asset| asset.id == asset_id)
            .is_some_and(asset_supports_files);
        if !supported {
            self.notify_rayops_error(
                self.i18n.t("ssh.rayops.files_unsupported"),
                cx,
            );
            return;
        }
        self.rayops_files = RayOpsFileState::open(asset_id);
        cx.notify();
        self.reload_rayops_directory(cx);
    }

    /// Navigates the file view to what the user typed into the path field.
    ///
    /// A relative path is refused rather than resolved: it would resolve against this process's
    /// working directory, not the asset's, and the gateway would then be asked about a directory
    /// the user never named.
    pub(in crate::workspace) fn submit_rayops_path(&mut self, cx: &mut Context<Self>) {
        let typed = self
            .session_manager
            .read(cx)
            .rayops_path_draft
            .trim()
            .to_owned();
        // Focus is released either way: leaving keystrokes bound to the field after the user has
        // acted on it would swallow the next key they press.
        self.session_manager.update(cx, |manager, cx| {
            manager.focused_input = None;
            cx.notify();
        });
        if typed.is_empty() {
            return;
        }
        if !typed.starts_with('/') {
            self.notify_rayops_error(self.i18n.t("ssh.rayops.files_path_must_be_absolute"), cx);
            return;
        }
        self.navigate_rayops_directory(typed, cx);
    }

    /// Closes the file view.
    pub(in crate::workspace) fn close_rayops_files(&mut self, cx: &mut Context<Self>) {
        self.rayops_files = RayOpsFileState::default();
        cx.notify();
    }

    /// Loads the directory the view is currently pointed at.
    pub(in crate::workspace) fn reload_rayops_directory(&mut self, cx: &mut Context<Self>) {
        let Some(asset_id) = self.rayops_files.asset_id else {
            return;
        };
        let Some(request) = self.rayops_file_request(asset_id) else {
            self.rayops_files.phase = RayOpsFilePhase::Failed;
            self.rayops_files.error = Some(self.i18n.t("ssh.rayops.files_no_session"));
            cx.notify();
            return;
        };
        let path = self.rayops_files.path.clone();
        self.rayops_files.phase = RayOpsFilePhase::Loading;
        self.rayops_files.error = None;
        cx.notify();

        let runtime = self.forwarding_runtime.clone();
        let task = cx.spawn(async move |this, cx| {
            let outcome = runtime
                .spawn(super::super::rayops_flow::list_rayops_directory(request, path))
                .await;
            let _ = this.update_in(cx, |this, _window, cx| {
                match outcome {
                    Ok(Ok(listing)) => {
                        // The gateway echoes the path it actually read, which can differ from the
                        // one asked for, so the view follows the answer rather than the request.
                        if !listing.path.is_empty() {
                            this.rayops_files.path = listing.path;
                        }
                        this.rayops_files.entries = listing.entries;
                        this.rayops_files.phase = RayOpsFilePhase::Ready;
                        this.rayops_files.error = None;
                        this.rayops_files.selected = None;
                    }
                    Ok(Err(message)) => {
                        this.rayops_files.phase = RayOpsFilePhase::Failed;
                        // A host-key refusal is the gateway's own problem, not the user's and not
                        // this client's, and its raw form ("knownhosts: key is unknown") reads like
                        // something the operator did wrong. Saying whose problem it is is the whole
                        // value of the message.
                        this.rayops_files.error = Some(if message.contains("knownhosts") {
                            this.i18n.t("ssh.rayops.files_host_key_unverified")
                        } else {
                            message
                        });
                    }
                    Err(join) => {
                        this.rayops_files.phase = RayOpsFilePhase::Failed;
                        this.rayops_files.error =
                            Some(format!("the RayOps file call failed: {join}"));
                    }
                }
                cx.notify();
            });
        });
        task.detach();
    }

    /// Navigates to a directory.
    pub(in crate::workspace) fn navigate_rayops_directory(
        &mut self,
        path: String,
        cx: &mut Context<Self>,
    ) {
        self.rayops_files.path = path;
        self.rayops_files.selected = None;
        self.reload_rayops_directory(cx);
    }

    /// Moves the view to its parent directory. Does nothing at the root.
    pub(in crate::workspace) fn rayops_files_go_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = parent_path(&self.rayops_files.path) {
            self.navigate_rayops_directory(parent, cx);
        }
    }

    /// Selects an entry by index.
    pub(in crate::workspace) fn select_rayops_file(&mut self, index: usize, cx: &mut Context<Self>) {
        self.rayops_files.selected = Some(index);
        cx.notify();
    }

    /// Downloads the selected entry to `local_path`.
    ///
    /// A file is fetched as one response; a directory is walked. The gateway offers neither a
    /// recursive nor an archive endpoint, so the walk is this client's job — many round trips
    /// rather than one, which is why the result reports how many files it took.
    pub(in crate::workspace) fn download_rayops_selected(
        &mut self,
        local_path: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(asset_id) = self.rayops_files.asset_id else {
            return;
        };
        let Some(entry) = self.rayops_files.selected_entry().cloned() else {
            return;
        };
        let Some(request) = self.rayops_file_request(asset_id) else {
            return;
        };
        let is_directory = entry.kind == oxideterm_rayops::SftpEntryKind::Directory;
        let remote_path = entry.path.clone();

        let runtime = self.forwarding_runtime.clone();
        let task = cx.spawn(async move |this, cx| {
            if is_directory {
                let outcome = runtime
                    .spawn(super::super::rayops_flow::download_rayops_tree(
                        request,
                        remote_path,
                        local_path.clone(),
                    ))
                    .await;
                let _ = this.update_in(cx, |this, _window, cx| match outcome {
                    Ok(Ok(summary)) => {
                        // Skipped entries are reported, not silently dropped: a partial download
                        // that looks complete is worse than one that says what it left out.
                        if !summary.skipped.is_empty() {
                            this.notify_rayops_error(
                                format!(
                                    "{}: {}",
                                    this.i18n.t("ssh.rayops.files_skipped_entries"),
                                    summary.skipped.join(", ")
                                ),
                                cx,
                            );
                        }
                    }
                    Ok(Err(message)) => this.notify_rayops_error(message, cx),
                    Err(join) => {
                        this.notify_rayops_error(format!("the RayOps download failed: {join}"), cx)
                    }
                });
                return;
            }

            let outcome = runtime
                .spawn(super::super::rayops_flow::download_rayops_file(
                    request,
                    remote_path,
                ))
                .await;
            let _ = this.update_in(cx, |this, _window, cx| match outcome {
                Ok(Ok(bytes)) => {
                    // Written here rather than in the caller so a failed write is reported through
                    // the same channel as a failed download.
                    if let Err(error) = std::fs::write(&local_path, &bytes) {
                        this.notify_rayops_error(
                            format!("cannot write {}: {error}", local_path.display()),
                            cx,
                        );
                    }
                }
                Ok(Err(message)) => this.notify_rayops_error(message, cx),
                Err(join) => {
                    this.notify_rayops_error(format!("the RayOps download failed: {join}"), cx)
                }
            });
        });
        task.detach();
    }

    /// Uploads `local_path` into the directory on display, then reloads it.
    pub(in crate::workspace) fn upload_rayops_file(
        &mut self,
        local_path: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(asset_id) = self.rayops_files.asset_id else {
            return;
        };
        let Some(request) = self.rayops_file_request(asset_id) else {
            return;
        };
        let directory = self.rayops_files.path.clone();
        let file_name = local_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if file_name.is_empty() {
            return;
        }
        // The gateway joins this onto `path`, so a bare filename uploads into the current
        // directory rather than somewhere else.
        let relative_path = file_name.clone();

        let runtime = self.forwarding_runtime.clone();
        let task = cx.spawn(async move |this, cx| {
            let outcome = runtime
                .spawn(super::super::rayops_flow::upload_rayops_file(
                    request,
                    directory,
                    relative_path,
                    local_path,
                ))
                .await;
            let _ = this.update_in(cx, |this, _window, cx| {
                match outcome {
                    Ok(Ok(())) => this.reload_rayops_directory(cx),
                    Ok(Err(message)) => this.notify_rayops_error(message, cx),
                    Err(join) => {
                        this.notify_rayops_error(format!("the RayOps upload failed: {join}"), cx)
                    }
                }
                cx.notify();
            });
        });
        task.detach();
    }

    /// Creates a directory inside the one on display.
    ///
    /// Takes the name rather than prompting: the sidebar has no inline text input yet, and a file
    /// view that silently did nothing when "new directory" was pressed would be worse than one
    /// without the button. Wiring is the next step.
    #[allow(dead_code)]
    pub(in crate::workspace) fn create_rayops_directory_here(
        &mut self,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let Some(asset_id) = self.rayops_files.asset_id else {
            return;
        };
        let Some(request) = self.rayops_file_request(asset_id) else {
            return;
        };
        let path = child_path(&self.rayops_files.path, &name);
        let runtime = self.forwarding_runtime.clone();
        let task = cx.spawn(async move |this, cx| {
            let outcome = runtime
                .spawn(super::super::rayops_flow::create_rayops_directory(
                    request, path,
                ))
                .await;
            let _ = this.update_in(cx, |this, _window, cx| {
                match outcome {
                    Ok(Ok(())) => this.reload_rayops_directory(cx),
                    Ok(Err(message)) => this.notify_rayops_error(message, cx),
                    Err(join) => this
                        .notify_rayops_error(format!("the RayOps mkdir failed: {join}"), cx),
                }
                cx.notify();
            });
        });
        task.detach();
    }

    /// Deletes the selected entry, then reloads the directory.
    pub(in crate::workspace) fn delete_rayops_selected(&mut self, cx: &mut Context<Self>) {
        let Some(asset_id) = self.rayops_files.asset_id else {
            return;
        };
        let Some(remote_path) = self
            .rayops_files
            .selected_entry()
            .map(|entry| entry.path.clone())
        else {
            return;
        };
        let Some(request) = self.rayops_file_request(asset_id) else {
            return;
        };
        let runtime = self.forwarding_runtime.clone();
        let task = cx.spawn(async move |this, cx| {
            let outcome = runtime
                .spawn(super::super::rayops_flow::delete_rayops_entry(
                    request,
                    remote_path,
                ))
                .await;
            let _ = this.update_in(cx, |this, _window, cx| {
                match outcome {
                    Ok(Ok(())) => this.reload_rayops_directory(cx),
                    Ok(Err(message)) => {
                        // The gateway refuses a non-empty directory with a bare
                        // `SSH_FX_FAILURE`, which says nothing an operator can act on. The
                        // refusal is the safe behaviour — nothing is deleted — so it is worth
                        // stating plainly: delete a directory's contents first.
                        let explained = if message.contains("SSH_FX_FAILURE") {
                            this.i18n.t("ssh.rayops.files_directory_not_empty")
                        } else {
                            message
                        };
                        this.notify_rayops_error(explained, cx);
                    }
                    Err(join) => {
                        this.notify_rayops_error(format!("the RayOps delete failed: {join}"), cx)
                    }
                }
                cx.notify();
            });
        });
        task.detach();
    }

    /// Renames the selected entry within its current directory.
    ///
    /// Same reason as [`Self::create_rayops_directory_here`]: the name has to come from an inline
    /// input that does not exist yet.
    #[allow(dead_code)]
    pub(in crate::workspace) fn rename_rayops_selected(
        &mut self,
        new_name: String,
        cx: &mut Context<Self>,
    ) {
        let Some(asset_id) = self.rayops_files.asset_id else {
            return;
        };
        let Some(old_path) = self
            .rayops_files
            .selected_entry()
            .map(|entry| entry.path.clone())
        else {
            return;
        };
        let Some(request) = self.rayops_file_request(asset_id) else {
            return;
        };
        let new_path = child_path(&self.rayops_files.path, &new_name);
        let runtime = self.forwarding_runtime.clone();
        let task = cx.spawn(async move |this, cx| {
            let outcome = runtime
                .spawn(super::super::rayops_flow::rename_rayops_entry(
                    request, old_path, new_path,
                ))
                .await;
            let _ = this.update_in(cx, |this, _window, cx| {
                match outcome {
                    Ok(Ok(())) => this.reload_rayops_directory(cx),
                    Ok(Err(message)) => this.notify_rayops_error(message, cx),
                    Err(join) => {
                        this.notify_rayops_error(format!("the RayOps rename failed: {join}"), cx)
                    }
                }
                cx.notify();
            });
        });
        task.detach();
    }
}

impl crate::workspace::WorkspaceApp {
    /// Asks for a local directory, then downloads the selected file into it.
    pub(in crate::workspace) fn prompt_download_rayops_selected(&mut self, cx: &mut Context<Self>) {
        let Some(entry_name) = self
            .rayops_files
            .selected_entry()
            .map(|entry| entry.name.clone())
        else {
            return;
        };
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(gpui::SharedString::from(self.i18n.t("ssh.rayops.files_choose_folder"))),
        });
        let target = async move {
            match receiver.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next().map(|dir| dir.join(entry_name)),
                _ => None,
            }
        };
        let task = cx.spawn(async move |this, cx| {
            // A cancelled picker is not a failure, so nothing is reported.
            if let Some(path) = target.await {
                let _ = this.update_in(cx, |this, _window, cx| {
                    this.download_rayops_selected(path, cx);
                });
            }
        });
        task.detach();
    }

    /// Asks for a local file, then uploads it into the directory on display.
    pub(in crate::workspace) fn prompt_upload_rayops_file(&mut self, cx: &mut Context<Self>) {
        if !self.rayops_files.is_open() {
            return;
        }
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(gpui::SharedString::from(self.i18n.t("ssh.rayops.files_choose_file"))),
        });
        let selection = async move {
            match receiver.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            }
        };
        let task = cx.spawn(async move |this, cx| {
            if let Some(path) = selection.await {
                let _ = this.update_in(cx, |this, _window, cx| {
                    this.upload_rayops_file(path, cx);
                });
            }
        });
        task.detach();
    }
}
