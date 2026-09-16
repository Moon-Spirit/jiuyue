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
    ClientEnvelope, ClientEvent, MessageAck, MessageRejected, NewMessage, Ping, Resume, Resync,
    ResyncReason, SendMessage, ServerEnvelope, ServerEvent,
};
use tokio::sync::mpsc;

use crate::replay::ReplayBuffer;
use crate::{RealtimeError, RealtimeHub};

/// One connection: the authenticated User, the hub it belongs to, and the writer.
///
/// A struct rather than a bag of arguments keeps frame handling to one parameter
/// and makes `user_id` / `hub` available exactly where the send path needs them.
pub(crate) struct Session<'a> {
    user_id: &'a str,
    hub: &'a RealtimeHub,
    writer: ConnectionWriter,
}

impl<'a> Session<'a> {
    /// Assemble a session for a freshly registered connection.
    ///
    /// `connection_id` is the id the registry assigned; it is echoed in every
    /// heartbeat so a client can prove which connection a resume position belongs
    /// to.
    pub(crate) fn new(
        user_id: &'a str,
        hub: &'a RealtimeHub,
        tx: mpsc::Sender<ServerEnvelope>,
        replay_capacity: usize,
        connection_id: u64,
    ) -> Self {
        Self {
            user_id,
            hub,
            writer: ConnectionWriter::new(tx, replay_capacity, connection_id),
        }
    }

    /// Enqueue a heartbeat.
    pub(crate) async fn send_ping(&mut self) -> Result<(), RealtimeError> {
        self.writer.send_ping().await
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
        }

        Ok(())
    }

    /// Persist a submitted Message, fan out a newly written one, and build the reply.
    async fn deliver_message(&self, send: SendMessage) -> ServerEvent {
        match self
            .hub
            .chat()
            .send_message(self.user_id, send.clone())
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
