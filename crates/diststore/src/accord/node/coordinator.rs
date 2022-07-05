use crate::accord::keys::{Key, KeySet};
use crate::accord::protocol::{
    Accept, AcceptOk, Apply, Commit, PreAccept, PreAcceptOk, Read, ReadOk, StartExecuteInternal,
};
use crate::accord::timestamp::{Timestamp, TimestampProvider};
use crate::accord::topology::TopologyManagerRef;
use crate::accord::transaction::{Transaction, TransactionId, TransactionKind};
use crate::accord::{AccordError, ComputeData, Executor, NodeId, Result};
use std::collections::{HashMap, HashSet};

/// State specific for coordinating transactions.
#[derive(Debug)]
pub struct CoordinatorState<K> {
    tm: TopologyManagerRef,
    /// All transactions initiated by this coordinator.
    transactions: HashMap<TransactionId, CoordinatedTransaction<K>>,
    ts_provider: TimestampProvider,
}

impl<K: Key> CoordinatorState<K> {
    pub fn new(tm: TopologyManagerRef, node: NodeId) -> Self {
        let ts_provider = TimestampProvider::new(node);
        CoordinatorState {
            tm,
            ts_provider,
            transactions: HashMap::new(),
        }
    }

    /// Create a new read transaction.
    pub fn new_read_tx(&mut self, keys: KeySet<K>, command: Vec<u8>) -> PreAccept<K> {
        self.new_tx(keys, command, TransactionKind::Read)
    }

    /// Create a new write transaction.
    pub fn new_write_tx(&mut self, keys: KeySet<K>, command: Vec<u8>) -> PreAccept<K> {
        self.new_tx(keys, command, TransactionKind::Write)
    }

    fn new_tx(&mut self, keys: KeySet<K>, command: Vec<u8>, kind: TransactionKind) -> PreAccept<K> {
        let ts = self.ts_provider.unique_now();
        let id = TransactionId(ts);
        let tx = Transaction::new(id.clone(), kind, keys, command);
        self.transactions
            .insert(id, CoordinatedTransaction::new(tx.clone()));

        PreAccept { tx }
    }

    /// Store a proposal from some replica.
    ///
    /// Analogous to Accord's "PreAcceptOk" routine.
    ///
    /// If a quorum has been reached, an "accept" or "commit" message will be
    /// returned depending on if the quorum is using the fast path or slow path.
    pub fn store_proposal(
        &mut self,
        from: NodeId,
        msg: PreAcceptOk,
    ) -> Result<Option<AcceptOrCommit<K>>> {
        let tx = self
            .transactions
            .get_mut(&msg.tx)
            .ok_or(AccordError::MissingTx(msg.tx.clone()))?;
        let received = tx.preaccept_msg_received(from, msg.proposed, msg.deps)?;
        let check = self.tm.get_current().check_quorum(received);

        // Good to commit with original timestamp.
        if tx.proposed_is_original() && check.have_fast_path {
            tx.move_to_executing()?;
            return Ok(Some(AcceptOrCommit::Commit(Commit {
                tx: tx.inner.clone(),
                timestamp: tx.proposed.clone(),
                deps: tx.deps.iter().cloned().collect(),
            })));
        }

        // Wait for some more messages before accepting. We might still get fast
        // path.
        if tx.proposed_is_original() {
            return Ok(None);
        }

        // Accept highest timestamp we have so far if we have quorum.
        if check.have_slow_path {
            tx.move_to_accepting()?;
            return Ok(Some(AcceptOrCommit::Accept(Accept {
                tx: tx.inner.clone(),
                timestamp: tx.proposed.clone(),
                deps: tx.deps.iter().cloned().collect(),
            })));
        }

        // No quorum yet.
        Ok(None)
    }

    /// Store an acknowledgement of accept for a node. Returns a commit message
    /// once a simple quorum has been reached.
    pub fn store_accept_ok(&mut self, from: NodeId, msg: AcceptOk) -> Result<Option<Commit<K>>> {
        let tx = self
            .transactions
            .get_mut(&msg.tx)
            .ok_or(AccordError::MissingTx(msg.tx.clone()))?;
        let received = tx.accept_msg_received(from, msg.deps)?;
        let check = self.tm.get_current().check_quorum(received);

        // Only need a simple quorum to commit.
        if check.have_slow_path {
            return Ok(Some(Commit {
                tx: tx.inner.clone(),
                timestamp: tx.proposed.clone(),
                deps: tx.deps.iter().cloned().collect(),
            }));
        }

        // Need more messages.
        Ok(None)
    }

    pub fn start_execute(&mut self, msg: StartExecuteInternal) -> Result<Read<K>> {
        let tx = self
            .transactions
            .get(&msg.tx)
            .ok_or(AccordError::MissingTx(msg.tx.clone()))?;
        // TODO: Need to track which shards we need to await reads from.
        Ok(Read {
            tx: tx.inner.clone(),
            timestamp: tx.proposed.clone(),
            deps: tx.deps.iter().cloned().collect(),
        })
    }

    pub fn store_read_ok<E>(&mut self, executor: &E, msg: ReadOk) -> Result<Option<Apply<K>>>
    where
        E: Executor<K>,
    {
        let tx = self
            .transactions
            .get(&msg.tx)
            .ok_or(AccordError::MissingTx(msg.tx.clone()))?;

        // TODO: Only compute if we've gotten messages from all shards. We're
        // only dealing with a single for shard now.
        let computed = executor
            .compute(&msg.data, &tx.proposed, &tx.inner)
            .map_err(|e| AccordError::ExecutorError(format!("compute: {:?}", e)))?;

        Ok(Some(Apply {
            tx: tx.inner.clone(),
            timestamp: tx.proposed.clone(),
            deps: tx.deps.iter().cloned().collect(),
            data: computed,
        }))
    }
}

#[derive(Debug)]
pub enum AcceptOrCommit<K> {
    Accept(Accept<K>),
    Commit(Commit<K>),
}

#[derive(Debug)]
enum TransactionStatus {
    /// Transaction is in the preaccepting phase, awaiting proposals from other
    /// replicas.
    PreAccepting { received: HashSet<NodeId> },
    /// Transaction is in the accepting phases, awaiting acks from a quorum of
    /// replicas.
    Accepting { received: HashSet<NodeId> },
    /// Transaction is in the execution protocol.
    Executing,
}

#[derive(Debug)]
struct CoordinatedTransaction<K> {
    inner: Transaction<K>,
    /// Max timestamp we've received so far.
    proposed: Timestamp,
    status: TransactionStatus,
    deps: HashSet<TransactionId>,
}

impl<K: Key> CoordinatedTransaction<K> {
    fn new(tx: Transaction<K>) -> Self {
        let proposed = tx.get_id().0.clone();
        CoordinatedTransaction {
            inner: tx,
            proposed,
            status: TransactionStatus::PreAccepting {
                received: HashSet::new(),
            },
            deps: HashSet::new(),
        }
    }

    fn proposed_is_original(&self) -> bool {
        self.proposed == self.inner.get_id().0
    }

    /// Add a node's preaccept proposal, returning a set of nodes we've received
    /// messages from so far.
    fn preaccept_msg_received(
        &mut self,
        from: NodeId,
        proposed: Timestamp,
        deps: Vec<TransactionId>,
    ) -> Result<&HashSet<NodeId>> {
        if matches!(self.status, TransactionStatus::PreAccepting { .. }) {
            self.merge_deps(deps);
        }
        match &mut self.status {
            TransactionStatus::PreAccepting { received } => {
                received.insert(from);
                if proposed > self.proposed {
                    self.proposed = proposed;
                }
                Ok(received)
            }
            other => Err(AccordError::InvalidTransactionState(format!("{:?}", other))),
        }
    }

    fn accept_msg_received(
        &mut self,
        from: NodeId,
        deps: Vec<TransactionId>,
    ) -> Result<&HashSet<NodeId>> {
        if matches!(self.status, TransactionStatus::Accepting { .. }) {
            self.merge_deps(deps);
        }
        match &mut self.status {
            TransactionStatus::Accepting { received } => {
                received.insert(from);
                Ok(received)
            }
            other => Err(AccordError::InvalidTransactionState(format!("{:?}", other))),
        }
    }

    fn move_to_accepting(&mut self) -> Result<()> {
        match &mut self.status {
            status @ TransactionStatus::PreAccepting { .. } => {
                *status = TransactionStatus::Accepting {
                    received: HashSet::new(),
                };
                Ok(())
            }
            other => Err(AccordError::InvalidTransactionState(format!("{:?}", other))),
        }
    }

    fn move_to_executing(&mut self) -> Result<()> {
        match &mut self.status {
            status @ TransactionStatus::PreAccepting { .. }
            | status @ TransactionStatus::Accepting { .. } => {
                *status = TransactionStatus::Accepting {
                    received: HashSet::new(),
                };
                Ok(())
            }
            other => Err(AccordError::InvalidTransactionState(format!("{:?}", other))),
        }
    }

    fn merge_deps<I>(&mut self, deps: I)
    where
        I: IntoIterator<Item = TransactionId>,
    {
        for dep in deps.into_iter() {
            self.deps.insert(dep);
        }
    }
}
