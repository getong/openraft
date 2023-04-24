use std::fmt::Debug;

use tokio::sync::oneshot;

use crate::core::sm;
use crate::engine::CommandKind;
use crate::error::Infallible;
use crate::error::InitializeError;
use crate::error::InstallSnapshotError;
use crate::progress::entry::ProgressEntry;
use crate::progress::Inflight;
use crate::raft::AppendEntriesResponse;
use crate::raft::InstallSnapshotResponse;
use crate::raft::VoteRequest;
use crate::raft::VoteResponse;
use crate::LeaderId;
use crate::LogId;
use crate::Node;
use crate::NodeId;
use crate::RaftTypeConfig;
use crate::SnapshotMeta;
use crate::Vote;

/// Commands to send to `RaftRuntime` to execute, to update the application state.
#[derive(Debug)]
pub(crate) enum Command<C>
where C: RaftTypeConfig
{
    /// Becomes a leader, i.e., its `vote` is granted by a quorum.
    /// The runtime initializes leader data when receives this command.
    BecomeLeader,

    /// No longer a leader. Clean up leader's data.
    QuitLeader,

    /// Append one entry.
    AppendEntry { entry: C::Entry },

    /// Append a `range` of entries.
    AppendInputEntries { entries: Vec<C::Entry> },

    /// Append a blank log.
    ///
    /// One of the usage is when a leader is established, a blank log is written to commit the
    /// state.
    AppendBlankLog { log_id: LogId<C::NodeId> },

    /// Replicate the committed log id to other nodes
    ReplicateCommitted { committed: Option<LogId<C::NodeId>> },

    /// Commit entries that are already in the store, upto `upto`, inclusive.
    /// And send applied result to the client that proposed the entry.
    LeaderCommit {
        // TODO: pass the log id list?
        // TODO: merge LeaderCommit and FollowerCommit
        already_committed: Option<LogId<C::NodeId>>,
        upto: LogId<C::NodeId>,
    },

    /// Commit entries that are already in the store, upto `upto`, inclusive.
    FollowerCommit {
        already_committed: Option<LogId<C::NodeId>>,
        upto: LogId<C::NodeId>,
    },

    /// Replicate log entries or snapshot to a target.
    Replicate {
        target: C::NodeId,
        req: Inflight<C::NodeId>,
    },

    /// Membership config changed, need to update replication streams.
    /// The Runtime has to close all old replications and start new ones.
    /// Because a replication stream should only report state for one membership config.
    /// When membership config changes, the membership log id stored in ReplicationCore has to be
    /// updated.
    RebuildReplicationStreams {
        /// Targets to replicate to.
        targets: Vec<(C::NodeId, ProgressEntry<C::NodeId>)>,
    },

    /// Save vote to storage
    SaveVote { vote: Vote<C::NodeId> },

    /// Send vote to all other members
    SendVote { vote_req: VoteRequest<C::NodeId> },

    /// Purge log from the beginning to `upto`, inclusive.
    PurgeLog { upto: LogId<C::NodeId> },

    /// Delete logs that conflict with the leader from a follower/learner since log id `since`,
    /// inclusive.
    DeleteConflictLog { since: LogId<C::NodeId> },

    // TODO(3): put all state machine related commands in a separate enum.
    /// Build a snapshot.
    ///
    /// This command will send a [`sm::Command::BuildSnapshot`] to [`sm::Worker`].
    /// The response will be sent back in a `RaftMsg::StateMachine` message to `RaftCore`.
    BuildSnapshot {},

    /// Install a snapshot data file: e.g., replace state machine with snapshot, save snapshot
    /// data.
    InstallSnapshot {
        snapshot_meta: SnapshotMeta<C::NodeId, C::Node>,
    },

    // TODO: remove this, use InstallSnapshot instead.
    /// A received snapshot does not need to be installed, just drop buffered snapshot data.
    CancelSnapshot {
        snapshot_meta: SnapshotMeta<C::NodeId, C::Node>,
    },

    /// Send result to caller
    Respond {
        when: Option<Condition<C::NodeId>>,
        resp: Respond<C::NodeId, C::Node>,
    },
}

/// For unit testing
impl<C> PartialEq for Command<C>
where
    C: RaftTypeConfig,
    C::Entry: PartialEq,
{
    #[rustfmt::skip]
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Command::BecomeLeader,                                Command::BecomeLeader)                                                        => true,
            (Command::QuitLeader,                                  Command::QuitLeader)                                                          => true,
            (Command::AppendEntry { entry },                       Command::AppendEntry { entry: b }, )                                          => entry == b,
            (Command::AppendInputEntries { entries },              Command::AppendInputEntries { entries: b }, )                                 => entries == b,
            (Command::AppendBlankLog { log_id },                   Command::AppendBlankLog { log_id: b }, )                                      => log_id == b,
            (Command::ReplicateCommitted { committed },            Command::ReplicateCommitted { committed: b }, )                               => committed == b,
            (Command::LeaderCommit { already_committed, upto, },   Command::LeaderCommit { already_committed: b_committed, upto: b_upto, }, )    => already_committed == b_committed && upto == b_upto,
            (Command::FollowerCommit { already_committed, upto, }, Command::FollowerCommit { already_committed: b_committed, upto: b_upto, }, )  => already_committed == b_committed && upto == b_upto,
            (Command::Replicate { target, req },                   Command::Replicate { target: b_target, req: other_req, }, )                   => target == b_target && req == other_req,
            (Command::RebuildReplicationStreams { targets },       Command::RebuildReplicationStreams { targets: b }, )                          => targets == b,
            (Command::SaveVote { vote },                           Command::SaveVote { vote: b })                                                => vote == b,
            (Command::SendVote { vote_req },                       Command::SendVote { vote_req: b }, )                                          => vote_req == b,
            (Command::PurgeLog { upto },                           Command::PurgeLog { upto: b })                                                => upto == b,
            (Command::DeleteConflictLog { since },                 Command::DeleteConflictLog { since: b }, )                                    => since == b,
            (Command::BuildSnapshot {},                            Command::BuildSnapshot { })                                                   => true,
            (Command::InstallSnapshot { snapshot_meta },           Command::InstallSnapshot { snapshot_meta: b }, )                              => snapshot_meta == b,
            (Command::CancelSnapshot { snapshot_meta },            Command::CancelSnapshot { snapshot_meta: b }, )                               => snapshot_meta == b,
            (Command::Respond { when, resp: send },                Command::Respond { when: b_when, resp: b })                                   => send == b && when == b_when,
            _ => false,
        }
    }
}

impl<C> Command<C>
where C: RaftTypeConfig
{
    #[allow(dead_code)]
    #[rustfmt::skip]
    pub(crate) fn kind(&self) -> CommandKind {
        match self {
            Command::BecomeLeader                     => CommandKind::Other,
            Command::QuitLeader                       => CommandKind::Other,
            Command::AppendEntry { .. }               => CommandKind::Log,
            Command::AppendInputEntries { .. }        => CommandKind::Log,
            Command::AppendBlankLog { .. }            => CommandKind::Log,
            Command::ReplicateCommitted { .. }        => CommandKind::Network,
            Command::LeaderCommit { .. }              => CommandKind::StateMachine,
            Command::FollowerCommit { .. }            => CommandKind::StateMachine,
            Command::Replicate { .. }                 => CommandKind::Network,
            Command::RebuildReplicationStreams { .. } => CommandKind::Other,
            Command::SaveVote { .. }                  => CommandKind::Log,
            Command::SendVote { .. }                  => CommandKind::Network,
            Command::PurgeLog { .. }                  => CommandKind::Log,
            Command::DeleteConflictLog { .. }         => CommandKind::Log,
            Command::BuildSnapshot { .. }             => CommandKind::StateMachine,
            Command::InstallSnapshot { .. }           => CommandKind::StateMachine,
            Command::CancelSnapshot { .. }            => CommandKind::Other,
            Command::Respond { .. }                   => CommandKind::Other,
        }
    }

    /// Return the condition the command waits for if any.
    #[allow(dead_code)]
    #[rustfmt::skip]
    pub(crate) fn condition(&self) -> Option<&Condition<C::NodeId>> {
        match self {
            Command::BecomeLeader                     => None,
            Command::QuitLeader                       => None,
            Command::AppendEntry { .. }               => None,
            Command::AppendInputEntries { .. }        => None,
            Command::AppendBlankLog { .. }            => None,
            Command::ReplicateCommitted { .. }        => None,
            Command::LeaderCommit { .. }              => None,
            Command::FollowerCommit { .. }            => None,
            Command::Replicate { .. }                 => None,
            Command::RebuildReplicationStreams { .. } => None,
            Command::SaveVote { .. }                  => None,
            Command::SendVote { .. }                  => None,
            Command::PurgeLog { .. }                  => None,
            Command::DeleteConflictLog { .. }         => None,
            Command::BuildSnapshot {}                 => None,
            Command::InstallSnapshot { .. }           => None,
            Command::CancelSnapshot { .. }            => None,
            Command::Respond { when, .. }             => when.as_ref(),
        }
    }
}

/// A condition to wait for before running a command.
#[derive(Debug, Clone, Copy)]
#[derive(PartialEq, Eq)]
pub(crate) enum Condition<NID>
where NID: NodeId
{
    /// Wait until the log is flushed to the disk.
    ///
    /// In raft, a log io can be uniquely identified by `(leader_id, log_id)`, not `log_id`.
    /// A same log id can be written multiple times by different leaders.
    #[allow(dead_code)]
    LogFlushed {
        leader: LeaderId<NID>,
        log_id: Option<LogId<NID>>,
    },

    /// Wait until the log is applied to the state machine.
    Applied { log_id: Option<LogId<NID>> },

    /// Wait until a [`sm::Worker`] command is finished.
    #[allow(dead_code)]
    StateMachineCommand { command_seq: sm::CommandSeq },
}

/// A command to send return value to the caller via a `oneshot::Sender`.
#[derive(Debug)]
#[derive(PartialEq, Eq)]
#[derive(derive_more::From)]
pub(crate) enum Respond<NID, N>
where
    NID: NodeId,
    N: Node,
{
    Vote(ValueSender<Result<VoteResponse<NID>, Infallible>>),
    AppendEntries(ValueSender<Result<AppendEntriesResponse<NID>, Infallible>>),
    ReceiveSnapshotChunk(ValueSender<Result<(), InstallSnapshotError>>),
    InstallSnapshot(ValueSender<Result<InstallSnapshotResponse<NID>, InstallSnapshotError>>),
    Initialize(ValueSender<Result<(), InitializeError<NID, N>>>),
}

impl<NID, N> Respond<NID, N>
where
    NID: NodeId,
    N: Node,
{
    pub(crate) fn new<T>(res: T, tx: oneshot::Sender<T>) -> Self
    where
        T: Debug + PartialEq + Eq,
        Self: From<ValueSender<T>>,
    {
        Respond::from(ValueSender::new(res, tx))
    }

    pub(crate) fn send(self) {
        match self {
            Respond::Vote(x) => x.send(),
            Respond::AppendEntries(x) => x.send(),
            Respond::ReceiveSnapshotChunk(x) => x.send(),
            Respond::InstallSnapshot(x) => x.send(),
            Respond::Initialize(x) => x.send(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ValueSender<T>
where T: Debug + PartialEq + Eq
{
    value: T,
    tx: oneshot::Sender<T>,
}

impl<T> PartialEq for ValueSender<T>
where T: Debug + PartialEq + Eq
{
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T> Eq for ValueSender<T> where T: Debug + PartialEq + Eq {}

impl<T> ValueSender<T>
where T: Debug + PartialEq + Eq
{
    pub(crate) fn new(res: T, tx: oneshot::Sender<T>) -> Self {
        Self { value: res, tx }
    }

    pub(crate) fn send(self) {
        let _ = self.tx.send(self.value);
    }
}
