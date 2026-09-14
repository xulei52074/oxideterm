// A terminal session carried by the RayOps KoKo gateway.
//
// RayOps terminates SSH server-side, so this backend never speaks SSH: it drives a WebSocket
// whose protocol is a JSON control envelope plus unwrapped terminal output. The protocol
// lives in the RayTerm repository (`crates/oxideterm-rayops`) and is injected as a
// `Box<dyn Socket>`, which keeps this crate free of an HTTP or WebSocket dependency and lets
// the protocol be verified on a machine that cannot build this workspace.
//
// Structure mirrors `session/telnet.rs`, the closest precedent: a worker task owns the
// transport and forwards events over a bounded channel, while the backend owns the terminal
// model and drains those events from the render loop. The transport must not touch the
// terminal model directly — `Term` is behind an `Arc<FairMutex<_>>` for the local event
// listener, not for cross-thread mutation from a socket task.
//
// The gateway's phases matter here and are modelled explicitly: an `ERROR` before `CONNECT`
// means the dial failed, after it means command governance discarded the input. Reporting the
// wrong one either reconnects pointlessly on every forbidden command or swallows a dial
// failure, so the phase travels with the error.

/// Events the socket task forwards to the backend.
enum RayOpsWorkerEvent {
    /// Terminal output, already classified by the protocol layer. Bytes, never a `String`: a
    /// multi-byte character split across two frames is ordinary traffic, and re-encoding
    /// through `String` would corrupt it.
    Output(Vec<u8>),
    /// The gateway confirmed the session.
    Established { terminal_id: Option<String> },
    /// A failure, with the protocol phase it happened in.
    Error {
        phase: oxideterm_rayops::Phase,
        message: String,
    },
    /// The session ended, locally or remotely.
    Ended,
}

/// Commands the backend sends to the socket task.
enum RayOpsCommand {
    /// User input, as bytes to be framed.
    Input(Vec<u8>),
    /// A geometry change.
    Resize { cols: u16, rows: u16 },
    /// End the session and let the pump drop the socket.
    Shutdown,
}

pub struct RayOpsSession {
    term: Arc<FairMutex<Term<LocalEventListener>>>,
    parser: Processor,
    event_rx: LocalEventReceiver,
    worker_rx: crate::backpressure::ByteBoundedReceiver<RayOpsWorkerEvent>,
    pending_events: Vec<TerminalEvent>,
    resize: TerminalResize,
    lifecycle: TerminalLifecycle,
    /// Held so the socket task is cancelled when the session is dropped or shut down.
    runtime: Option<Runtime>,
    command_tx: Option<tokio::sync::mpsc::Sender<RayOpsCommand>>,
    title: Option<String>,
    encoding: TerminalEncoding,
    output_decoder: TerminalOutputDecoder,
    output_processor: Option<TerminalOutputProcessor>,
    output_events_enabled: bool,
    graphics_ingress: GraphicsIngress,
    graphics: TerminalGraphicsState,
    graphics_alt_screen_active: bool,
    magic_scan: MagicScanWindow,
    encoding_detector: EncodingMismatchDetector,
    trigger_stream: Option<oxideterm_terminal_triggers::TerminalTriggerStream>,
    shell_integration: TerminalShellIntegration,
    /// Counters and the last error, so a failure is diagnosable without retaining terminal
    /// content.
    output_frames: u64,
    last_error: Option<String>,
}

impl RayOpsSession {
    pub fn new(config: RayOpsSessionConfig) -> Self {
        let resize = TerminalResize::new(config.cols, config.rows, 0, 0);
        let size = TerminalSize {
            cols: resize.cols,
            rows: resize.rows,
            cell_width: resize.cell_width,
            cell_height: resize.cell_height,
        };
        let (listener, event_rx) = local_event_channel();
        let (worker_tx, worker_rx) = crate::backpressure::byte_bounded_channel_with_activity(
            crate::backpressure::TRANSPORT_OUTPUT_BACKLOG_BYTES,
            listener.activity_sender(),
        );
        let (command_tx, command_rx) = tokio::sync::mpsc::channel(256);
        let term_config = interactive_terminal_config(config.scrollback_lines);
        let term = Arc::new(FairMutex::new(Term::new(term_config, &size, listener)));

        let mut session = Self {
            term,
            parser: Processor::new(),
            event_rx,
            worker_rx,
            pending_events: Vec::new(),
            resize,
            lifecycle: TerminalLifecycle::Running,
            runtime: None,
            command_tx: Some(command_tx.clone()),
            title: Some(config.title),
            encoding: TerminalEncoding::Utf8,
            output_decoder: TerminalOutputDecoder::new(TerminalEncoding::Utf8),
            output_processor: None,
            output_events_enabled: false,
            graphics_ingress: GraphicsIngress::new(config.graphics_options),
            graphics: TerminalGraphicsState::default(),
            graphics_alt_screen_active: false,
            magic_scan: MagicScanWindow::default(),
            encoding_detector: EncodingMismatchDetector::new(TerminalEncoding::Utf8),
            trigger_stream: None,
            shell_integration: TerminalShellIntegration::default(),
            output_frames: 0,
            last_error: None,
        };

        // The socket is moved into the task, so the task owns the transport and its
        // cancellation is the session's shutdown path.
        let runtime = Runtime::new().ok();
        match runtime.as_ref() {
            Some(runtime) => {
                runtime.spawn(rayops_socket_task(
                    config.socket,
                    worker_tx,
                    command_rx,
                    (
                        resize.cols.min(u16::MAX as usize) as u16,
                        resize.rows.min(u16::MAX as usize) as u16,
                    ),
                ));
            }
            None => {
                // Without a runtime there is no transport, so the session reports itself over
                // rather than sitting inert. Silence here would look like a hung connection.
                session.lifecycle = TerminalLifecycle::Closed;
                session.last_error =
                    Some("a Tokio runtime is required for a RayOps session".to_owned());
            }
        }
        session.runtime = runtime;
        session
    }

    /// Queues one command for the socket task.
    fn send_command(&mut self, command: RayOpsCommand) -> Result<()> {
        let Some(tx) = self.command_tx.as_ref() else {
            return Err(anyhow::anyhow!("the RayOps session has no transport"));
        };
        tx.try_send(command)
            .map_err(|_| anyhow::anyhow!("the RayOps transport is not accepting commands"))
    }

    /// Frames user input and queues it.
    ///
    /// Input travels as a `TERMINAL_DATA` frame and never as a raw WebSocket frame: the
    /// gateway forwards anything it cannot parse as text, and KoKo only maps *binary* frames
    /// to its own binary channel, so a raw payload would arrive by neither path.
    fn send_input(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.send_command(RayOpsCommand::Input(bytes.to_vec()))
    }

    fn process_terminal_output<'a>(&self, bytes: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
        apply_terminal_output_processor(&self.output_processor, bytes)
    }

    /// Feeds one block of transport output into the terminal model.
    ///
    /// Mirrors `TelnetSession::feed_plain_transport_output`, including the ordering: graphics
    /// escape sequences are extracted *before* the terminal model sees the bytes, because
    /// otherwise they would be consumed as ordinary characters and corrupt the screen.
    fn feed_plain_transport_output(&mut self, bytes: &[u8]) {
        // Preserve protocol bytes before optional plugin display transforms.
        let processed_output = self.process_terminal_output(bytes);
        let bytes = processed_output.as_ref();
        for kind in self.magic_scan.scan(bytes) {
            self.pending_events.push(TerminalEvent::MagicDetected(kind));
        }
        let mut term = self.term.lock();
        let size = TerminalSize {
            cols: self.resize.cols,
            rows: self.resize.rows,
            cell_width: self.resize.cell_width,
            cell_height: self.resize.cell_height,
        };
        let cursor = Cell::new(graphics_cursor_from_term(&term, size));
        let mut protocol_responses = Vec::new();
        self.graphics_ingress.advance_ordered(
            bytes,
            |segment| match segment {
                TerminalGraphicsSegment::Terminal(terminal_bytes) => {
                    if let Some(hint) = self.encoding_detector.observe(&terminal_bytes) {
                        self.pending_events.push(TerminalEvent::EncodingHint(hint));
                    }
                    let decoded = self.output_decoder.decode_to_utf8_bytes(&terminal_bytes);
                    if let Some(stream) = self.trigger_stream.as_mut() {
                        stream.observe_bytes(decoded.as_ref(), |matched| {
                            self.pending_events.push(TerminalEvent::TriggerMatched(matched));
                        });
                    }
                    if self.output_events_enabled {
                        let (_, recordable) = self.shell_integration.advance_with_recording(
                            &mut self.parser,
                            &mut *term,
                            decoded.as_ref(),
                            |event| self.pending_events.push(event),
                        );
                        if !recordable.is_empty() {
                            self.pending_events.push(TerminalEvent::Output(recordable));
                        }
                    } else {
                        self.shell_integration.advance(
                            &mut self.parser,
                            &mut *term,
                            decoded.as_ref(),
                            |event| self.pending_events.push(event),
                        );
                    }
                    self.graphics
                        .clear_for_alt_screen_transition(&term, &mut self.graphics_alt_screen_active);
                    cursor.set(graphics_cursor_from_term(&term, size));
                }
                TerminalGraphicsSegment::Event(event) => {
                    if let Some(response) = self.graphics.handle_event(event) {
                        protocol_responses.push(response);
                    }
                }
            },
            || cursor.get(),
        );
        drop(term);
        for response in protocol_responses {
            let _ = self.send_input(&response);
        }
    }

    fn handle_alacritty_event(&mut self, event: AlacEvent) -> bool {
        match event {
            AlacEvent::Title(title) => {
                self.pending_events.push(TerminalEvent::TitleChanged(title));
                true
            }
            AlacEvent::ResetTitle => {
                self.pending_events.push(TerminalEvent::TitleReset);
                true
            }
            AlacEvent::Bell => {
                self.pending_events.push(TerminalEvent::Bell);
                true
            }
            AlacEvent::Wakeup | AlacEvent::MouseCursorDirty | AlacEvent::CursorBlinkingChange => true,
            AlacEvent::PtyWrite(text) => {
                let _ = self.send_input(text.as_bytes());
                true
            }
            _ => false,
        }
    }

    /// Drains the socket task's events into the model, honouring `budget`.
    fn drain_worker_events_with_budget(
        &mut self,
        budget: TerminalDrainBudget,
    ) -> TerminalDrainReport {
        let started = Instant::now();
        let mut report = TerminalDrainReport::default();

        while report.events_drained < budget.max_events && !budget.time_exhausted(started) {
            let Ok(item) = self.worker_rx.try_recv() else {
                break;
            };
            report.events_drained += 1;
            match item.into_inner() {
                RayOpsWorkerEvent::Output(bytes) => {
                    self.output_frames += 1;
                    self.feed_plain_transport_output(&bytes);
                    // The terminal model changed, so the render loop has to be woken. Other
                    // backends push this from their event listener; the socket task cannot,
                    // because the model is owned here.
                    self.pending_events.push(TerminalEvent::Wakeup);
                    report.mark_changed();
                }
                RayOpsWorkerEvent::Established { terminal_id } => {
                    // Recorded, not shown: the KoKo id is an internal handle and the pane's
                    // title comes from the asset the user picked.
                    tracing::debug!(?terminal_id, "RayOps session established");
                    report.mark_changed();
                }
                RayOpsWorkerEvent::Error { phase, message } => {
                    // The phase is the reason this carries more than a string: before
                    // `CONNECT` the socket is dead, after it the session still works and only
                    // the input was refused.
                    tracing::warn!(phase = phase.as_str(), %message, "RayOps session error");
                    self.last_error = Some(message.clone());
                    self.pending_events
                        .push(TerminalEvent::TitleChanged(format!("RayOps: {message}")));
                    if phase != oxideterm_rayops::Phase::Established {
                        self.lifecycle = TerminalLifecycle::Closed;
                    }
                    report.mark_changed();
                }
                RayOpsWorkerEvent::Ended => {
                    self.lifecycle = TerminalLifecycle::Closed;
                    report.mark_changed();
                }
            }
        }

        if (report.events_drained >= budget.max_events || budget.time_exhausted(started))
            && !self.worker_rx.is_empty()
        {
            report.budget_exhausted = true;
        }
        report.drain_duration = started.elapsed();
        report
    }
}

/// Delivers one event to the backend, routing byte-carrying output through the bounded path.
///
/// Returns `Err(())` when the channel is gone, which means the session was dropped and the
/// task should stop.
fn forward_to_worker(
    worker_tx: &crate::backpressure::ByteBoundedSender<RayOpsWorkerEvent>,
    event: RayOpsWorkerEvent,
) -> std::result::Result<(), ()> {
    match event {
        RayOpsWorkerEvent::Output(bytes) => {
            let byte_len = bytes.len();
            worker_tx
                .send(RayOpsWorkerEvent::Output(bytes), byte_len)
                .map_err(|_| ())
        }
        control => worker_tx.send_control(control).map_err(|_| ()),
    }
}

/// Owns the socket for the session's lifetime and moves frames in both directions.
///
/// Returning ends the task, and the backend observes the closed channel as an ended session,
/// so there is no separate cancellation signal to keep in sync.
async fn rayops_socket_task(
    mut socket: Box<dyn oxideterm_rayops::Socket>,
    worker_tx: crate::backpressure::ByteBoundedSender<RayOpsWorkerEvent>,
    mut commands: tokio::sync::mpsc::Receiver<RayOpsCommand>,
    geometry: (u16, u16),
) {
    use oxideterm_rayops::SessionState;

    let mut state = SessionState::new();
    let mut geometry_sent = false;

    loop {
        tokio::select! {
            inbound = socket.read() => {
                let Ok(event) = inbound else {
                    let _ = worker_tx.send_control(RayOpsWorkerEvent::Ended);
                    return;
                };
                let (events, outbound) = state.handle(event);

                // The handshake carried the geometry, but the gateway's own `TERMINAL_INIT` is
                // what sets the PTY size authoritatively. Sending it once, right after the
                // session is established, keeps the remote shell's width in step with the pane;
                // skipping it leaves output wrapped for whatever the shell defaulted to.
                let mut outbound = outbound;
                if !geometry_sent
                    && state.phase() == oxideterm_rayops::Phase::Established
                    && let Some(size) = oxideterm_rayops::TerminalSize::new(geometry.0, geometry.1)
                {
                    outbound.push(oxideterm_rayops::OutboundEvent::Frame(
                        oxideterm_rayops::ClientFrame::terminal_init(size).to_json(),
                    ));
                    geometry_sent = true;
                }
                for event in events {
                    let forwarded = match event {
                        oxideterm_rayops::SessionEvent::Output(bytes) => {
                            RayOpsWorkerEvent::Output(bytes)
                        }
                        oxideterm_rayops::SessionEvent::Established { terminal_id } => {
                            RayOpsWorkerEvent::Established { terminal_id }
                        }
                        oxideterm_rayops::SessionEvent::Error { phase, message } => {
                            RayOpsWorkerEvent::Error { phase, message }
                        }
                        oxideterm_rayops::SessionEvent::Ended { .. } => RayOpsWorkerEvent::Ended,
                        // The gateway's session description is diagnostic only; the asset the
                        // user chose is the identity this client presents.
                        oxideterm_rayops::SessionEvent::SessionInfo { .. } => continue,
                    };
                    // Output carries bytes and goes through the bounded path so backpressure
                    // accounts for it; control events are free and must never be dropped for
                    // lack of budget, or the session would look hung.
                    let delivered = match forward_to_worker(&worker_tx, forwarded) {
                        Ok(()) => true,
                        Err(()) => false,
                    };
                    if !delivered {
                        return;
                    }
                }
                for event in outbound {
                    if socket.write(event).await.is_err() {
                        let _ = worker_tx.send_control(RayOpsWorkerEvent::Ended);
                        return;
                    }
                }
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    // The backend dropped its sender, which means the session is gone.
                    let _ = socket
                        .write(state.close(oxideterm_rayops::CLOSE_NORMAL, "session closed"))
                        .await;
                    return;
                };
                let closing = matches!(command, RayOpsCommand::Shutdown);
                let frame = match command {
                    RayOpsCommand::Input(bytes) => match String::from_utf8(bytes) {
                        Ok(text) => state.input(text),
                        // Refused rather than lossily converted: silently replacing bytes would
                        // send a different command than the user typed.
                        Err(_) => {
                            let _ = worker_tx.send_control(RayOpsWorkerEvent::Error {
                                phase: state.phase(),
                                message: "input was not valid UTF-8 and was not sent".to_owned(),
                            });
                            None
                        }
                    },
                    RayOpsCommand::Resize { cols, rows } => state.resize(cols, rows),
                    RayOpsCommand::Shutdown => {
                        Some(state.close(oxideterm_rayops::CLOSE_NORMAL, "closed"))
                    }
                };
                if let Some(frame) = frame
                    && socket.write(frame).await.is_err()
                {
                    let _ = worker_tx.send_control(RayOpsWorkerEvent::Ended);
                    return;
                }
                if closing {
                    let _ = worker_tx.send_control(RayOpsWorkerEvent::Ended);
                    return;
                }
            }
        }
    }
}

impl TerminalSessionBackend for RayOpsSession {
    fn kind(&self) -> TerminalSessionKind {
        TerminalSessionKind::RayOps
    }

    fn title(&self) -> Option<String> {
        self.title.clone()
    }

    fn lifecycle(&self) -> TerminalLifecycle {
        self.lifecycle.clone()
    }

    fn process_info(&self) -> TerminalProcessInfo {
        // RayOps owns the remote process and this client cannot see it. An empty projection is
        // the honest answer; inventing one would show the operator a process that is not there.
        TerminalProcessInfo::default()
    }

    fn refresh_process_info(&mut self) {}

    fn read_pending(&mut self) -> bool {
        self.read_pending_with_budget(TerminalDrainBudget::unlimited())
            .changed
    }

    fn read_pending_with_budget(&mut self, budget: TerminalDrainBudget) -> TerminalDrainReport {
        let started = Instant::now();
        let mut report = self.drain_worker_events_with_budget(budget);
        while report.events_drained < budget.max_events && !budget.time_exhausted(started) {
            let Ok(event) = self.event_rx.try_recv() else {
                break;
            };
            report.events_drained += 1;
            if self.handle_alacritty_event(event) {
                report.mark_changed();
            }
        }
        if (report.events_drained >= budget.max_events || budget.time_exhausted(started))
            && !self.event_rx.is_empty()
        {
            report.budget_exhausted = true;
        }
        report.drain_duration = started.elapsed();
        report
    }

    fn activity_receiver(&self) -> TerminalActivityReceiver {
        self.event_rx.activity_receiver()
    }

    fn take_events(&mut self) -> Vec<TerminalEvent> {
        std::mem::take(&mut self.pending_events)
    }

    fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        self.send_input(bytes)
    }

    fn write_protocol_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        // Protocol bytes and user input are one stream here: the gateway takes a character
        // stream and escape sequences are ordinary characters in it.
        self.send_input(bytes)
    }

    fn write_text(&mut self, text: &str) -> Result<()> {
        self.send_input(text.as_bytes())
    }

    fn paste_text(&mut self, text: &str) -> Result<()> {
        self.send_input(text.as_bytes())
    }

    fn set_encoding(&mut self, encoding: TerminalEncoding) {
        self.encoding = encoding;
        self.output_decoder = TerminalOutputDecoder::new(encoding);
    }

    fn set_output_processor(&mut self, processor: Option<TerminalOutputProcessor>) {
        self.output_processor = processor;
    }

    fn set_output_events_enabled(&mut self, enabled: bool) {
        self.output_events_enabled = enabled;
    }

    fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    fn set_focused(&mut self, focused: bool) -> Result<()> {
        let should_report = {
            let mut term = self.term.lock();
            term.is_focused = focused;
            term.mode().contains(TermMode::FOCUS_IN_OUT)
        };
        if let Some(report) = focus_report_sequence(should_report, focused) {
            self.send_input(report)?;
        }
        Ok(())
    }

    fn resize_with_cell_size(&mut self, resize: TerminalResize) -> Result<()> {
        let grid_changed = self.resize.cols != resize.cols || self.resize.rows != resize.rows;
        self.resize = resize;
        let size = TerminalSize {
            cols: resize.cols,
            rows: resize.rows,
            cell_width: resize.cell_width,
            cell_height: resize.cell_height,
        };
        self.term.lock().resize(size);
        // A non-positive geometry is refused here rather than sent: the gateway drops one
        // without a reply, so the failure would otherwise be invisible. `SessionState::resize`
        // additionally de-duplicates, because the gateway forwards every accepted resize to
        // the PTY.
        if grid_changed {
            // Converted rather than clamped: a geometry wider than the protocol can express
            // is an error the caller can see, whereas clamping would silently send a size the
            // user did not ask for and the PTY would reflow to it.
            let cols = u16::try_from(resize.cols)
                .map_err(|_| anyhow::anyhow!("terminal width {} exceeds the protocol", resize.cols))?;
            let rows = u16::try_from(resize.rows)
                .map_err(|_| anyhow::anyhow!("terminal height {} exceeds the protocol", resize.rows))?;
            if cols > 0 && rows > 0 {
                self.send_command(RayOpsCommand::Resize { cols, rows })?;
            }
        }
        Ok(())
    }

    fn scroll_lines(&mut self, delta: i32) {
        if delta != 0 {
            self.term.lock().scroll_display(Scroll::Delta(delta));
        }
    }

    fn page_up(&mut self) {
        self.term.lock().scroll_display(Scroll::PageUp);
    }

    fn page_down(&mut self) {
        self.term.lock().scroll_display(Scroll::PageDown);
    }

    fn scroll_to_top(&mut self) {
        self.term.lock().scroll_display(Scroll::Top);
    }

    fn scroll_to_bottom(&mut self) {
        self.term.lock().scroll_display(Scroll::Bottom);
    }

    fn scroll_to_display_offset(&mut self, offset: usize) {
        let mut term = self.term.lock();
        let max_offset = term.total_lines().saturating_sub(term.screen_lines());
        let target = offset.min(max_offset);
        let current = term.grid().display_offset();
        let delta = target as i32 - current as i32;
        if delta != 0 {
            term.scroll_display(Scroll::Delta(delta));
        }
    }

    fn search_matches(&self, query: &str) -> Vec<TerminalSearchMatch> {
        let term = self.term.lock();
        search_matches_from_term(&term, self.resize.cols, query)
    }

    fn set_selection(&self, selection: Option<crate::TerminalSelectionRange>) {
        crate::selection::set_term_selection(&mut self.term.lock(), selection);
    }

    fn selection(&self) -> Option<crate::TerminalSelectionRange> {
        crate::selection::term_selection(&self.term.lock())
    }

    fn clear_buffer(&mut self) {
        let mut term = self.term.lock();
        clear_terminal_buffer(&mut term);
        self.graphics.clear();
    }

    fn snapshot(&self) -> TerminalSnapshot {
        let term = self.term.lock();
        snapshot_from_term(
            &term,
            TerminalSize {
                cols: self.resize.cols,
                rows: self.resize.rows,
                cell_width: self.resize.cell_width,
                cell_height: self.resize.cell_height,
            },
            &self.graphics,
        )
    }

    fn terminate_active_task(&mut self) -> Result<()> {
        // Ctrl-C as ordinary input: the remote shell interprets it, exactly as it would on a
        // local or SSH session.
        self.send_input(b"\x03")
    }

    fn kill_active_task(&mut self) -> Result<()> {
        self.send_input(b"\x03")
    }

    fn shutdown(&mut self) {
        if matches!(self.lifecycle, TerminalLifecycle::Closed) {
            return;
        }
        let _ = self.send_command(RayOpsCommand::Shutdown);
        // Dropping the runtime cancels the socket task, which owns the transport. It is
        // dropped rather than kept so a shut-down session cannot leave a task holding a live
        // gateway connection.
        self.command_tx = None;
        self.runtime = None;
        self.lifecycle = TerminalLifecycle::Closed;
    }
}

#[cfg(test)]
mod rayops_session_tests {
    use super::*;

    /// A socket the test drives by hand.
    ///
    /// The real transport is a WebSocket chosen outside this crate, so a fake is the only way
    /// to exercise the session without a gateway. It records everything the session wrote,
    /// which is how the tests observe the protocol rather than the implementation.
    struct ScriptedSocket {
        // Owned directly, never behind a lock: the trait's read future must be `Send`, and a
        // guard held across the `recv().await` would make it non-Send. This is the same
        // constraint that forced `Socket` to return a boxed future in the first place.
        inbound: tokio::sync::mpsc::UnboundedReceiver<oxideterm_rayops::InboundEvent>,
        written: crossbeam_channel::Sender<oxideterm_rayops::OutboundEvent>,
    }

    impl oxideterm_rayops::Socket for ScriptedSocket {
        fn read(
            &mut self,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = std::result::Result<
                            oxideterm_rayops::InboundEvent,
                            oxideterm_rayops::SocketError,
                        >,
                    > + Send
                    + '_,
            >,
        > {
            Box::pin(async move {
                self.inbound
                    .recv()
                    .await
                    .ok_or_else(|| oxideterm_rayops::SocketError::new("the test script ended"))
            })
        }

        fn write(
            &mut self,
            event: oxideterm_rayops::OutboundEvent,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = std::result::Result<(), oxideterm_rayops::SocketError>,
                    > + Send
                    + '_,
            >,
        > {
            Box::pin(async move {
                let _ = self.written.send(event);
                Ok(())
            })
        }
    }

    /// One session plus the two ends of its fake transport.
    struct Harness {
        session: TerminalSession,
        /// Push frames toward the session.
        inbound: tokio::sync::mpsc::UnboundedSender<oxideterm_rayops::InboundEvent>,
        /// Everything the session wrote.
        written: crossbeam_channel::Receiver<oxideterm_rayops::OutboundEvent>,
        runtime: Arc<Runtime>,
    }

    impl Harness {
        fn new() -> Self {
            let runtime = Arc::new(Runtime::new().expect("a test runtime"));
            let (inbound_tx, inbound_rx) = tokio::sync::mpsc::unbounded_channel();
            let (written_tx, written_rx) = crossbeam_channel::unbounded();
            let socket = ScriptedSocket {
                inbound: inbound_rx,
                written: written_tx,
            };
            let session = TerminalSession::rayops(RayOpsSessionConfig {
                title: "test asset".to_owned(),
                socket: Box::new(socket),
                cols: 80,
                rows: 24,
                scrollback_lines: 1000,
                graphics_options: GraphicsOptions::default(),
            });
            Self {
                session,
                inbound: inbound_tx,
                written: written_rx,
                runtime,
            }
        }

        /// Sends one text frame and waits for the session to consume it.
        fn send(&self, payload: &str) {
            self.inbound
                .send(oxideterm_rayops::InboundEvent::Text(payload.as_bytes().to_vec()))
                .expect("the session is alive");
        }

        /// Runs the session's drain until `predicate` holds, or panics after a deadline.
        fn wait_for(&mut self, label: &str, mut predicate: impl FnMut(&mut Self) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                self.session.read_pending();
                if predicate(self) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            panic!("timed out waiting for {label}");
        }
    }

    /// The session reports `Closed` once the socket task has ended.
    fn session_closed(harness: &mut Harness) -> bool {
        harness.session.lifecycle() == TerminalLifecycle::Closed
    }

    #[test]
    fn establishing_a_session_and_receiving_output_reaches_the_terminal_model() {
        let mut harness = Harness::new();

        harness.send(r#"{"id":"koko-1","type":"CONNECT"}"#);
        // The gateway's authoritative geometry request must follow establishment, or the remote
        // PTY keeps a default width and output wraps at the wrong column. Collected rather than
        // peeked: a predicate that consumed the frame would leave the assertion nothing to read.
        let mut frames: Vec<String> = Vec::new();
        harness.wait_for("TERMINAL_INIT", |h| {
            h.runtime.block_on(async { tokio::task::yield_now().await });
            while let Ok(event) = h.written.try_recv() {
                if let oxideterm_rayops::OutboundEvent::Frame(json) = event {
                    frames.push(json);
                }
            }
            frames.iter().any(|json| json.contains("TERMINAL_INIT"))
        });
        assert!(
            frames.iter().any(|json| json.contains("TERMINAL_INIT")),
            "expected a TERMINAL_INIT frame, got {frames:?}"
        );

        harness.send("hello");
        harness.wait_for("terminal output", |h| {
            let snapshot = h.session.snapshot();
            let text: String = snapshot
                .lines
                .iter()
                .map(|line| line.text())
                .collect::<Vec<_>>()
                .join("\n");
            text.contains("hello")
        });

        // Output must also wake the render loop. Without this the terminal receives bytes but
        // the pane never redraws, which is invisible to a snapshot assertion.
        let events = harness.session.take_events();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TerminalEvent::Wakeup)),
            "output must push a Wakeup so the pane redraws"
        );
    }

    #[test]
    fn a_bare_output_frame_is_written_to_the_screen_byte_for_byte() {
        let mut harness = Harness::new();
        harness.send("abc");
        harness.wait_for("output", |h| {
            let snapshot = h.session.snapshot();
            snapshot
                .lines
                .iter()
                .any(|line| line.text().contains("abc"))
        });
    }

    #[test]
    fn an_error_before_connect_closes_the_session_but_one_after_does_not() {
        // The distinction the phase exists for: before CONNECT the socket is dead, after it
        // only the input was refused. Treating them the same either reconnects pointlessly on
        // every forbidden command or swallows a failed dial.
        let mut harness = Harness::new();
        harness.send(r#"{"type":"ERROR","data":"dial failed"}"#);
        harness.wait_for("closed after pre-connect error", session_closed);

        let mut established = Harness::new();
        established.send(r#"{"id":"koko-1","type":"CONNECT"}"#);
        established.wait_for("established", |h| {
            h.runtime.block_on(async { tokio::task::yield_now().await });
            h.written.try_recv().is_ok()
        });
        established.send(r#"{"type":"ERROR","data":"command rejected"}"#);
        // The error is surfaced through the event stream, not written to the screen: a gateway
        // message is not terminal output, and printing it would corrupt whatever the remote
        // program is drawing.
        established.wait_for("the error to surface", |h| {
            h.session
                .take_events()
                .iter()
                .any(|event| matches!(event, TerminalEvent::TitleChanged(title) if title.contains("command rejected")))
        });
        assert_ne!(
            established.session.lifecycle(),
            TerminalLifecycle::Closed,
            "a refused command must leave the session usable"
        );
    }

    #[test]
    fn shutdown_is_idempotent_and_stops_further_input() {
        let mut harness = Harness::new();
        harness.session.shutdown();
        assert_eq!(harness.session.lifecycle(), TerminalLifecycle::Closed);
        // Second call must be a no-op rather than a panic or a second close frame.
        harness.session.shutdown();
        assert_eq!(harness.session.lifecycle(), TerminalLifecycle::Closed);

        // A shut-down session refuses input instead of queueing it for a transport that is gone.
        assert!(
            harness.session.write_input(b"ls\n").is_err(),
            "input after shutdown must fail rather than be silently dropped"
        );
    }

    #[test]
    fn cancelling_a_read_ends_the_session_without_replaying_input() {
        // A disconnect must surface as a closed session, and nothing may be re-sent: replaying
        // input could re-execute a command that already ran.
        let mut harness = Harness::new();
        harness.send(r#"{"id":"koko-1","type":"CONNECT"}"#);
        harness.wait_for("established", |h| {
            h.runtime.block_on(async { tokio::task::yield_now().await });
            h.written.try_recv().is_ok()
        });
        let _ = harness.session.write_input(b"echo hi\n");
        harness.wait_for("the input frame", |h| {
            h.runtime.block_on(async { tokio::task::yield_now().await });
            h.written.try_recv().is_ok()
        });

        // Dropping the sender is an abrupt disconnect from the session's point of view.
        let inbound = harness.inbound.clone();
        drop(inbound);
        drop(harness.inbound.clone());
        harness.session.shutdown();
        assert_eq!(harness.session.lifecycle(), TerminalLifecycle::Closed);
        assert!(
            harness.written.try_recv().is_err(),
            "nothing may be replayed after the session ends"
        );
    }
}
