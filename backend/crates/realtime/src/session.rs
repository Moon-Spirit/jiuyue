//! One connection's state machine: sequencing, replay, and the resume handshake.
//!
//! [`Session`] owns everything about a live socket that is not the transport loop
//! itself: the connection sequence `s`, the bounded [`ReplayBuffer`], the outbound
//! queue, and the interpretation of a client frame. The transport loop in
//! [`crate::connection`] drives it.
//!
//! # Sequencing
//!
//! [`ConnectionWriter`] keeps allocating the next `s`, recording an envelope for
//! replay, and handing it to the writer as one operation, so no code path can send
//! an envelope the replay buffer never saw — which would make a later replay punch
//! a hole in the client's gap arithmetic.

use std::time::{SystemTime, UNIX_EPOCH};

use jiuyue_contract::{
    ClientEnvelope, ClientEvent, MarkRead, MessageAck, MessageRejected, NewMessage, Ping,
    ReadMarker, ReadReceipt, Resume, Resync, ResyncReason, SendMessage, ServerEnvelope,
    ServerEvent, SyncCursor, SyncState,
};
use tokio::sync::mpsc;

use crate::cursor::CursorCheckpoint;
use crate::replay::ReplayBuffer;
use crate::{Device, RealtimeError, RealtimeHub};

/// One connection: the authenticated Device, the hub it belongs to, and the writer.
///
/// A struct rather than a bag of arguments keeps frame handling to one parameter
/// and makes the Device / hub available exactly where the send path needs them.
///
/// `cursors` is the Sync Cursor write-coalescing buffer (see
/// [`CursorCheckpoint`]): one entry per Conversation, holding the highest position
/// this connection has been told the Device consumed. The buffer is flushed on the
/// heartbeat, on teardown, and eagerly once it holds a whole batch of distinct
/// Conversations; on a failed write it is kept, so the next flush retries instead
/// of losing progress.
pub(crate) struct Session<'a> {
    device: &'a Device,
    hub: &'a RealtimeHub,
    writer: ConnectionWriter,
    cursors: CursorCheckpoint,
}

impl<'a> Session<'a> {
    /// Assemble a session for a freshly registered connection.
    ///
    /// `connection_id` is the id the registry assigned; it is echoed in every
    /// heartbeat so a client can prove which connection a resume position belongs
    /// to.
    pub(crate) fn new(
        device: &'a Device,
        hub: &'a RealtimeHub,
        tx: mpsc::Sender<ServerEnvelope>,
        replay_capacity: usize,
        connection_id: u64,
    ) -> Self {
        Self {
            device,
            hub,
            writer: ConnectionWriter::new(tx, replay_capacity, connection_id),
            cursors: CursorCheckpoint::default(),
        }
    }

    /// Enqueue a heartbeat.
    pub(crate) async fn send_ping(&mut self) -> Result<(), RealtimeError> {
        self.writer.send_ping().await
    }

    /// Push the Device's stored Sync Cursors, when it has any.
    ///
    /// This is the "resume/sync response" of the delivery protocol, and it is
    /// deliberately silent for a Device with no stored progress: there is nothing
    /// to resume, so the client keeps its pre-cursor behaviour (load the newest
    /// page) and no frame is spent. A read failure is logged and swallowed — a
    /// database hiccup must degrade the *hint*, not refuse the connection, because
    /// the Conversation stream is still reachable over REST either way.
    pub(crate) async fn send_sync_state(&mut self) -> Result<(), RealtimeError> {
        let cursors = match self
            .hub
            .chat()
            .list_sync_cursors(&self.device.session_id)
            .await
        {
            Ok(cursors) => cursors,
            Err(error) => {
                tracing::warn!(%error, "could not load the device sync cursors");
                return Ok(());
            }
        };

        if cursors.is_empty() {
            return Ok(());
        }

        self.send_event(ServerEvent::SyncState(SyncState { cursors }))
            .await
    }

    /// Coalesce one reported Sync Cursor, checkpointing early if the buffer fills.
    ///
    /// The buffer keeps the highest position per Conversation, so a report that
    /// arrives out of order (or a replay) cannot rewind it. Reports are never
    /// persisted one at a time: [`Self::flush_cursors`] is the only writer.
    pub(crate) async fn record_cursor(&mut self, cursor: &SyncCursor) {
        if self.cursors.record(cursor) {
            self.flush_cursors().await;
        }
    }

    /// Persist every coalesced cursor the Device has reported.
    ///
    /// Best effort on purpose: a failure keeps the buffer so the next flush (the
    /// heartbeat, or teardown) retries, and the Device merely re-fetches the tail
    /// if the process dies first. The buffer is cleared only after a successful
    /// write, so a checkpoint is never lost silently.
    pub(crate) async fn flush_cursors(&mut self) {
        if self.cursors.is_empty() {
            return;
        }

        let batch = self.cursors.batch();
        match self
            .hub
            .chat()
            .save_sync_cursors(&self.device.session_id, &batch)
            .await
        {
            Ok(()) => self.cursors.clear(),
            Err(error) => {
                tracing::warn!(
                    %error,
                    pending = batch.len(),
                    "could not checkpoint device sync cursors; will retry"
                );
            }
        }
    }

    /// Enqueue an event, allocating its connection sequence.
    pub(crate) async fn send_event(&mut self, event: ServerEvent) -> Result<(), RealtimeError> {
        self.writer.send_event(event).await
    }

    /// Handle one decoded client frame.
    ///
    /// An unparseable frame is logged and ignored rather than dropping the
    /// connection — a client bug must not cost the user their session. A
    /// `SendMessage` is persisted inline: a connection processes its own sends in
    /// order, and the database call is bounded by the pool's acquire timeout, so
    /// one user's send cannot stall another's.
    pub(crate) async fn handle_frame(&mut self, text: &str) -> Result<(), RealtimeError> {
        let envelope = match serde_json::from_str::<ClientEnvelope>(text) {
            Ok(envelope) => envelope,
            Err(error) => {
                tracing::debug!(%error, "ignoring an undecodable client frame");
                return Ok(());
            }
        };

        match envelope.event() {
            ClientEvent::Ping(ping) => {
                tracing::trace!(seq = ping.seq, time_ms = ping.time_ms, "client heartbeat");
            }
            ClientEvent::SendMessage(send) => {
                let reply = self.deliver_message(send.clone()).await;
                self.send_event(reply).await?;
            }
            ClientEvent::Resume(resume) => {
                self.resume(resume).await?;
            }
            ClientEvent::SyncCursor(cursor) => {
                self.record_cursor(cursor).await;
            }
            ClientEvent::MarkRead(mark) => {
                self.mark_read(mark).await;
            }
        }

        Ok(())
    }

    /// Persist a submitted Message, fan out a newly written one, and build the reply.
    async fn deliver_message(&self, send: SendMessage) -> ServerEvent {
        match self
            .hub
            .chat()
            .send_message(&self.device.user_id, send.clone())
            .await
        {
            Ok(sent) => {
                // Fan out only when this call actually wrote the Message. A
                // replayed Client Message ID (or the loser of a race between two
                // identical sends) must be a no-op on the wire: everyone still gets
                // exactly one NewMessage for the Message, and the sender alone
                // still gets its ack — which may be the only reason it retried.
                if sent.created {
                    self.hub
                        .registry()
                        .deliver(
                            &sent.participants,
                            &ServerEvent::NewMessage(NewMessage {
                                message: sent.message.clone(),
                            }),
                        )
                        .await;
                }

                ServerEvent::MessageAck(MessageAck {
                    client_msg_id: send.client_msg_id.clone(),
                    message: sent.message,
                })
            }
            Err(error) => {
                tracing::debug!(%error, "rejected a message send");
                ServerEvent::MessageRejected(MessageRejected {
                    client_msg_id: send.client_msg_id.clone(),
                    code: error.code(),
                    message: error.message().to_owned(),
                })
            }
        }
    }

    /// Record that this User has read a Conversation, then fan the two positions
    /// out to their **disjoint** audiences.
    ///
    /// This is the one place the private Read Marker and the public Read Receipt
    /// meet, and they leave it by different doors:
    ///
    /// - [`ServerEvent::ReadMarker`] carries the private position and the caller's
    ///   Unread Count, and is delivered to **exactly the reader's own User** — so
    ///   every Device of the account clears its badge, and no other Participant
    ///   can ever receive it. The recipient list is `[update.user_id]` and nothing
    ///   else.
    /// - [`ServerEvent::ReadReceipt`] carries the public position and the reader's
    ///   identity, and is delivered to **exactly `update.others`** — the
    ///   Participants who are *not* the reader. The private marker is never placed
    ///   in this event.
    ///
    /// A failure is logged and swallowed: reading is advisory state, and a database
    /// hiccup must not cost the user their connection.
    async fn mark_read(&self, mark: &MarkRead) {
        let update = match self
            .hub
            .chat()
            .mark_read(&self.device.user_id, mark.clone())
            .await
        {
            Ok(update) => update,
            Err(error) => {
                tracing::debug!(%error, "rejected a read marker");
                return;
            }
        };

        // Private: the reader's own Devices only. Never `others`.
        self.hub
            .registry()
            .deliver(
                std::slice::from_ref(&update.user_id),
                &ServerEvent::ReadMarker(ReadMarker {
                    conversation_id: update.conversation_id.clone(),
                    last_read_seq: update.read_marker_seq,
                    unread_count: update.unread_count,
                }),
            )
            .await;

        // Public: the other Participants only. Never the reader's Devices, and the
        // value is the receipt column, not the marker.
        if !update.others.is_empty() {
            self.hub
                .registry()
                .deliver(
                    &update.others,
                    &ServerEvent::ReadReceipt(ReadReceipt {
                        conversation_id: update.conversation_id.clone(),
                        reader_id: update.user_id.clone(),
                        last_read_seq: update.read_receipt_seq,
                    }),
                )
                .await;
        }
    }

    /// Answer a [`ClientEvent::Resume`] with the "wake up and re-sync" outcome.
    ///
    /// On a replay the missed envelopes go out first, so the client's `s` is
    /// contiguous again before the [`Resync`] answer is enqueued after them.
    async fn resume(&mut self, resume: &Resume) -> Result<(), RealtimeError> {
        match self.writer.resume_plan(resume) {
            ResumePlan::Fresh => {
                tracing::debug!(last_seq = resume.last_seq, "fresh realtime connection");
                self.send_event(resync(ResyncReason::Fresh, 0)).await
            }
            ResumePlan::Replay(envelopes) => {
                let replayed = self.writer.replay(envelopes).await?;
                tracing::debug!(
                    last_seq = resume.last_seq,
                    replayed,
                    "replayed missed envelopes"
                );
                self.send_event(resync(ResyncReason::Replayed, replayed))
                    .await
            }
            ResumePlan::Unavailable => {
                tracing::debug!(
                    last_seq = resume.last_seq,
                    "replay buffer cannot fill the gap; a conversation repair is required"
                );
                self.send_event(resync(ResyncReason::Unavailable, 0)).await
            }
        }
    }
}

/// One connection's sequencing, replay buffer and outbound queue together.
struct ConnectionWriter {
    tx: mpsc::Sender<ServerEnvelope>,
    sequence: u64,
    replay: ReplayBuffer,
    /// The id of this connection, echoed in every heartbeat so a client can prove
    /// which connection a resume position belongs to.
    connection_id: u64,
}

impl ConnectionWriter {
    fn new(tx: mpsc::Sender<ServerEnvelope>, replay_capacity: usize, connection_id: u64) -> Self {
        Self {
            tx,
            sequence: 0,
            replay: ReplayBuffer::new(replay_capacity),
            connection_id,
        }
    }

    /// Allocate the next connection sequence and enqueue `event`.
    async fn send_event(&mut self, event: ServerEvent) -> Result<(), RealtimeError> {
        self.sequence += 1;

        let envelope = ServerEnvelope::new(self.sequence, now_ms()?, event);
        self.enqueue(envelope).await
    }

    /// Allocate the next connection sequence and enqueue a heartbeat.
    async fn send_ping(&mut self) -> Result<(), RealtimeError> {
        self.sequence += 1;

        let time_ms = now_ms()?;
        let envelope = ServerEnvelope::new(
            self.sequence,
            time_ms,
            ServerEvent::Ping(Ping {
                seq: self.sequence,
                time_ms,
                connection_id: Some(self.connection_id),
            }),
        );
        self.enqueue(envelope).await
    }

    /// Record an envelope for replay, then hand it to the writer.
    async fn enqueue(&mut self, envelope: ServerEnvelope) -> Result<(), RealtimeError> {
        self.replay.record(&envelope);
        self.tx
            .send(envelope)
            .await
            .map_err(|_| RealtimeError::Closed)
    }

    /// Re-send already-sequenced envelopes verbatim.
    ///
    /// No new sequence is allocated and nothing is re-recorded: a replay must be
    /// indistinguishable from the original delivery, or the client's gap
    /// arithmetic would be corrupted by the repair itself.
    async fn replay(&mut self, envelopes: Vec<ServerEnvelope>) -> Result<u64, RealtimeError> {
        let replayed = envelopes.len() as u64;

        for envelope in envelopes {
            self.tx
                .send(envelope)
                .await
                .map_err(|_| RealtimeError::Closed)?;
        }

        Ok(replayed)
    }

    /// Decide what a [`ClientEvent::Resume`] can be answered with.
    ///
    /// A `last_seq` of zero is a first connection: there is nothing to replay and
    /// nothing was missed. Otherwise the resume position must both name **this**
    /// connection and still be retained; anything else is unprovable — most
    /// importantly a position from a previous connection, which is exactly the
    /// reconnect case that must trigger a Conversation repair.
    fn resume_plan(&self, resume: &Resume) -> ResumePlan {
        if resume.last_seq == 0 {
            return ResumePlan::Fresh;
        }

        if resume.connection_id != Some(self.connection_id) {
            return ResumePlan::Unavailable;
        }

        match self.replay.after(resume.last_seq) {
            Some(envelopes) => ResumePlan::Replay(envelopes),
            None => ResumePlan::Unavailable,
        }
    }
}

/// What the server can do about a client's resume position.
enum ResumePlan {
    /// First connection, or an explicit `last_seq == 0`.
    Fresh,
    /// The missed envelopes, still retained (possibly an empty slice).
    Replay(Vec<ServerEnvelope>),
    /// The position can no longer be proven; repair Conversations instead.
    Unavailable,
}

/// Build the [`Resync`] event for an outcome.
fn resync(reason: ResyncReason, replayed: u64) -> ServerEvent {
    ServerEvent::Resync(Resync { reason, replayed })
}

/// Current wall-clock time as milliseconds since the Unix epoch.
fn now_ms() -> Result<i64, RealtimeError> {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH)?;
    i64::try_from(elapsed.as_millis()).map_err(|_| RealtimeError::ClockRange)
}
