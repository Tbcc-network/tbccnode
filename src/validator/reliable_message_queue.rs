use std::{
    collections::{BinaryHeap, HashMap},
    fmt, fmt::Formatter,
    sync::Arc,
    time::Duration, time::SystemTime
};
use std::cmp::Reverse;

use ton_api::ton::ton_node::{
    RempMessageStatus, RempMessageLevel,
    rempmessagestatus::{RempAccepted, RempIgnored, RempRejected}, RmqRecord
};
use ton_block::{ShardIdent, Message, BlockIdExt, ValidatorDescr};
use ton_types::{UInt256, Result, fail};
use catchain::{PrivateKey, PublicKey};
use crate::{
    engine_traits::EngineOperations,
    ext_messages::{get_level_and_level_change, is_finally_accepted, is_finally_rejected},
    validator::{
        mutex_wrapper::MutexWrapper,
        message_cache::{RmqMessage, MessageCache},
        remp_manager::RempManager,
        remp_block_parser::{process_block_messages_by_blockid, BlockProcessor},
        remp_catchain::{RempCatchainInfo, RempCatchainInstance}
    }
};
use failure::err_msg;
use ton_api::ton::ton_node::rmqrecord::RmqMessageDigest;

#[derive(Debug,PartialEq,Eq,PartialOrd,Ord,Clone)]
enum MessageQueueStatus { Created, Starting, Active, Stopping }
const RMQ_STOP_POLLING_INTERVAL: Duration = Duration::from_millis(1);

struct MessageQueueImpl {
    status: MessageQueueStatus,

    /// Messages in queue: true if message is not collated/in block, but waiting for collator
    /// invocation (in collation_order), false if message is already given to collator
    pending_collation_set: HashMap<UInt256, bool>,

    /// Messages, waiting for collator invocation
    pending_collation_order: BinaryHeap<(Reverse<u32>, UInt256)>,
}

pub struct MessageQueue {
    remp_manager: Arc<RempManager>,
    engine: Arc<dyn EngineOperations>,
    catchain_info: Arc<RempCatchainInfo>,
    catchain_instance: RempCatchainInstance,
    queues: MutexWrapper<MessageQueueImpl>,
}

impl MessageQueueImpl {
    pub fn add_to_collation_queue(&mut self, message_id: &UInt256, timestamp: u32, add_if_absent: bool) -> Result<(bool,usize)> {
        match self.pending_collation_set.get_mut(message_id) {
            Some(waiting_collation) if *waiting_collation => Ok((false, self.pending_collation_set.len())),
            Some(waiting_collation) => {
                self.pending_collation_order.push((Reverse(timestamp), message_id.clone()));
                *waiting_collation = true;
                Ok((true, self.pending_collation_set.len()))
            },
            None if add_if_absent => {
                self.pending_collation_set.insert(message_id.clone(), true);
                self.pending_collation_order.push((Reverse(timestamp), message_id.clone())); // Max heap --- need earliest message
                Ok((true, self.pending_collation_set.len()))
            }
            None =>
                fail!("Message {} is not present in collation set, cannot return it to collation queue", message_id)
        }
    }

    pub fn take_first_for_collation(&mut self) -> Result<Option<(UInt256, u32)>> {
        if let Some((timestamp, id)) = self.pending_collation_order.pop() {
            match self.pending_collation_set.insert(id.clone(), false) {
                None => fail!("Message {:?}: taken from collation queue, but not found in collation set", id),
                Some(false) => fail!("Message {:?}: already removed from collation queue and given to collator", id),
                Some(true) => Ok(Some((id, timestamp.0)))
            }
        }
        else {
            Ok(None)
        }
    }

    pub fn list_pending_for_forwarding(&mut self) -> Result<Vec<UInt256>> {
        return Ok(self.pending_collation_set.keys().cloned().collect())
    }
}

impl MessageQueue {
    pub fn create(
        engine: Arc<dyn EngineOperations>,
        remp_manager: Arc<RempManager>,
        shard: ShardIdent,
        catchain_seqno: u32,
        master_cc_seqno: u32,
        curr: &Vec<ValidatorDescr>,
        next: &Vec<ValidatorDescr>,
        local: &PublicKey
    ) -> Result<Self> {
        let remp_catchain_info = Arc::new(RempCatchainInfo::create(
            //rt, engine.clone(), remp_manager.clone(),
            catchain_seqno, master_cc_seqno, curr, next, local,
            shard.clone()
        )?);

        let remp_catchain_instance = RempCatchainInstance::new(remp_catchain_info.clone());

        let queues = MutexWrapper::with_metric(
            MessageQueueImpl {
                status: MessageQueueStatus::Created,
                pending_collation_set: HashMap::new(),
                pending_collation_order: BinaryHeap::new(),
            },
            format!("<<RMQ {}>>", remp_catchain_instance),
            #[cfg(feature = "telemetry")]
            engine.remp_core_telemetry().rmq_catchain_mutex_metric(&shard),
        );

        log::trace!(target: "remp", "Creating MessageQueue {}", remp_catchain_instance);

        return Ok(Self {
            remp_manager,
            engine,
            catchain_info: remp_catchain_info.clone(),
            catchain_instance: remp_catchain_instance,
            queues: queues,
        });
    }

    // pub fn get_id(&self) -> UInt256 {
    //     return self.queue_id;
    // }

    async fn set_queue_status(&self, required_status: MessageQueueStatus, new_status: MessageQueueStatus) -> Result<()> {
        self.queues.execute_sync(|mut q| {
            if q.status != required_status {
                fail!("RMQ {}: MessageQueue status is {:?}, but required to be {:?}", self, q.status, required_status)
            }
            else {
                q.status = new_status;
                Ok(())
            }
        }).await
    }

    pub fn send_response_to_fullnode(&self, rmq_message: Arc<RmqMessage>, status: RempMessageStatus) {
        log::debug!(target: "remp", "RMQ {}: queueing response to fullnode {}, status {}",
            self, rmq_message, status
        );
        if let Err(e) = self.remp_manager.queue_response_to_fullnode(
            self.catchain_info.local_key_id.clone(), rmq_message.clone(), status.clone()
        ) {
            log::error!(target: "remp", "RMQ {}: cannot queue response to fullnode: {}, {}, local key {:x}, error `{}`",
                self, rmq_message, status, self.catchain_info.local_key_id, e
            );
        }
    }

    pub fn update_status_send_response(&self, msgid: &UInt256, message: Arc<RmqMessage>, new_status: RempMessageStatus) {
        match self.remp_manager.message_cache.update_message_status(&msgid, new_status.clone()) {
            Ok(Some(final_status)) => self.send_response_to_fullnode(message.clone(), final_status),
            Ok(None) => (), // Send nothing, no status update is requested
            Err(e) => log::error!(target: "remp", 
                "RMQ {}: Cannot update status for {:x}, new status {}, error {}",
                self, msgid, new_status, e
            )
        }
    }

    pub async fn update_status_send_response_by_id(&self, msgid: &UInt256, new_status: RempMessageStatus) -> Option<Arc<RmqMessage>> {
        let message = self.get_message(msgid);
        match &message {
            Some(rm) => self.update_status_send_response(msgid, rm.clone(), new_status.clone()),
            None => 
                log::error!(target: "remp",
                    "RMQ {}: cannot find message {:x} in RMQ messages hash! (new status {})",
                    self, msgid, new_status
                )
        };
        message
    }
/*
    pub fn put_to_catchain(&self, rmq_message: Arc<RmqMessage>) {
        if let Err(e) = self.catchain_instance.pending_messages_queue_send(rmq_message.as_rmq_record(self.catchain_info.master_cc_seqno)) {
            log::error!(target: "remp",
                "RMQ {}: Cannot write reject message for {:x} into catchain: {}", self, rmq_message.message_id, e
            )
        }
    }
*/
    pub async fn start (self: Arc<MessageQueue>, local_key: PrivateKey) -> Result<()> {
        self.set_queue_status(MessageQueueStatus::Created, MessageQueueStatus::Starting).await?;

        log::trace!(target: "remp", "RMQ {}: starting", self);
 //     log::trace!(target: "remp", "RMQ {}: catchain created", self);
        let catchain_instance_impl = self.remp_manager.catchain_store.start_catchain(
            self.engine.clone(), self.remp_manager.clone(), self.catchain_info.clone(), local_key
        ).await?;
        log::trace!(target: "remp", "RMQ {}: catchain started", self);
        self.catchain_instance.init_instance(catchain_instance_impl)?;
        self.set_queue_status(MessageQueueStatus::Starting, MessageQueueStatus::Active).await
    }

    pub async fn stop(&self) -> Result<()> {
        loop {
            let (do_stop, do_break) = self.queues.execute_sync(|mut q| {
                match q.status {
                    MessageQueueStatus::Created  => { q.status = MessageQueueStatus::Stopping; Ok((false, true)) },
                    MessageQueueStatus::Starting => Ok((false, false)),
                    MessageQueueStatus::Stopping => fail!("RMQ {}: cannot stop queue with 'Stopping' status", self),
                    MessageQueueStatus::Active   => { q.status = MessageQueueStatus::Stopping; Ok((true, true)) },
                }
            }).await?;

            if do_stop {
                log::trace!(target: "remp", "RMQ {}: stopping catchain", self);
                return self.remp_manager.catchain_store.stop_catchain(&self.catchain_info.queue_id).await;
            }
            if do_break {
                log::trace!(target: "remp", "RMQ {}: catchain is not started -- skip stopping", self);
                return Ok(());
            }
            log::trace!(target: "remp", "RMQ {}: waiting for catchain to stop it", self);
            tokio::time::sleep(RMQ_STOP_POLLING_INTERVAL).await
        }
    }

    pub async fn put_message_to_rmq(&self, old_message: Arc<RmqMessage>) -> Result<()> {
        let msg = Arc::new(old_message.new_with_updated_source_idx(self.catchain_info.local_idx as u32));
        log::trace!(target: "remp", "Point 3. Pushing to RMQ {}; message {}", self, msg);
        self.catchain_instance.pending_messages_queue_send(msg.as_rmq_record(self.catchain_info.master_cc_seqno))?;

        #[cfg(feature = "telemetry")]
        self.engine.remp_core_telemetry().in_channel_to_catchain(
            &self.catchain_info.shard, self.catchain_instance.pending_messages_queue_len()?);

        if let Some(session) = &self.remp_manager.catchain_store.get_catchain_session(&self.catchain_info.queue_id).await {
            log::trace!(target: "remp", "Point 3. Activating RMQ {} processing", self);
            session.request_new_block(SystemTime::now());

            // Temporary status "New" --- message is not registered yet
            self.send_response_to_fullnode(msg, RempMessageStatus::TonNode_RempNew);
            Ok(())
        }
        else {
            log::error!(target: "remp", "RMQ {} not started", self);
            Err(failure::err_msg("RMQ is not started"))
        }
    }

    async fn add_pending_collation(&self, rmq_message: Arc<RmqMessage>, status_to_send: Option<RempMessageStatus>) -> Result<()> {
        let (added_to_queue, len) = self.queues.execute_sync(
            |catchain| catchain.add_to_collation_queue(
                &rmq_message.message_id, rmq_message.timestamp, true
            )
        ).await?;
        #[cfg(feature = "telemetry")]
        self.engine.remp_core_telemetry().pending_collation(&self.catchain_info.shard, len);

        if added_to_queue {
            log::trace!(target: "remp",
                "Point 5. RMQ {}: adding message {} to collator queue", self, rmq_message
            );
            self.remp_manager.message_cache.mark_collation_attempt(&rmq_message.message_id)?;
            if let Some(status) = status_to_send {
                self.send_response_to_fullnode(rmq_message.clone(), status);
            }
        }

        Ok(())
    }

    fn is_final_status(status: &RempMessageStatus) -> bool {
        match status {
            RempMessageStatus::TonNode_RempRejected(_) |
            RempMessageStatus::TonNode_RempTimeout |
            RempMessageStatus::TonNode_RempAccepted(RempAccepted { level: RempMessageLevel::TonNode_RempMasterchain, .. }) => true,

            _ => false
        }
    }

    fn get_status_level(status: &RempMessageStatus) -> RempMessageLevel {
        let (lvl,_) = get_level_and_level_change(status);
        lvl
    }

    fn get_status_blockid(status: &RempMessageStatus) -> BlockIdExt {
        match status {
            RempMessageStatus::TonNode_RempNew => BlockIdExt::default(),
            RempMessageStatus::TonNode_RempAccepted(a) => a.block_id.clone(),
            RempMessageStatus::TonNode_RempRejected(r) => r.block_id.clone(),
            RempMessageStatus::TonNode_RempIgnored(i) => i.block_id.clone(),
            RempMessageStatus::TonNode_RempTimeout => BlockIdExt::default(),
            RempMessageStatus::TonNode_RempDuplicate(b) => b.block_id.clone(),
            RempMessageStatus::TonNode_RempSentToValidators(_) => BlockIdExt::default(),
        }
    }

    pub fn is_session_active(&self) -> bool {
        self.catchain_instance.is_session_active()
    }

    /// Check received messages queue and put all received messages into
    /// hash map. Check status of all old messages in the hash map.
    pub async fn poll(&self) -> Result<()> {
        log::debug!(target: "remp", "Point 4. RMQ {}: polling; total {} messages in cache, {} messages in rmq_queue",
            self, 
            self.received_messages_count().await,
            self.catchain_instance.rmq_catchain_receiver_len()?
        );

        // Process new messages from RMQ catchain and send them to collator
        'queue_loop: loop {
            match self.catchain_instance.rmq_catchain_try_recv() {
                Err(e) => {
                    log::error!(target: "remp", "RMQ {}: error receiving from rmq_queue: `{}`", self, e);
                    break 'queue_loop;
                },
                Ok(None) => {
                    log::trace!(target: "remp", "RMQ {}: No more messages in rmq_queue", self);
                    break 'queue_loop;
                },
                Ok(Some(ton_api::ton::ton_node::RmqRecord::TonNode_RmqMessage(rmq_record_message))) => {
                    let rmq_message = Arc::new(RmqMessage::from_rmq_record(&rmq_record_message)?);
                    let rmq_message_master_seqno = rmq_record_message.masterchain_seqno as u32;
                    let forwarded = self.catchain_info.master_cc_seqno > rmq_message_master_seqno;

                    log::trace!(target: "remp",
                        "Point 4. RMQ {}: inserting new message {} from RMQ into message_cache, forwarded {}, message_master_cc {}",
                        self, rmq_message, forwarded, rmq_message_master_seqno
                    );

                    let added = self.remp_manager.message_cache.add_external_message_status(
                        &rmq_message.message_id,Some(rmq_message.clone()), self.catchain_info.shard.clone(), RempMessageStatus::TonNode_RempNew, rmq_message_master_seqno
                    ).await;

                    match added {
                        Err(e) => {
                            log::error!(target: "remp",
                                "Point 4. RMQ {}: cannot insert new message {} into message_cache, error: `{}`",
                                self, rmq_message, e
                            );
                        },
                        Ok(Some(old_status)) if forwarded && !Self::is_final_status(&old_status) => {
                            log::trace!(target: "remp",
                                "Point 4. RMQ {}. Message {} master_cc_seqno {} from previous set, validator {} - we have it with non-final status {}, try to collate",
                                self, rmq_message.message_id.to_hex_string(), rmq_message_master_seqno, rmq_message.source_idx, old_status
                            );
                            let ignored = RempIgnored { level: Self::get_status_level(&old_status), block_id: Self::get_status_blockid(&old_status) };
                            let new_status = RempMessageStatus::TonNode_RempIgnored(ignored);
                            self.remp_manager.message_cache.update_message_status(&rmq_message.message_id, new_status.clone())?;
                            self.add_pending_collation(rmq_message, Some(new_status)).await?;

                            #[cfg(feature = "telemetry")]
                            self.engine.remp_core_telemetry().add_to_cache_attempt(false);
                        }
                        Ok(Some(old_status)) => {
                            log::trace!(target: "remp",
                                "Point 4. RMQ {}. Message {} master_cc_seqno {} from validator {} - we already have it with status {}, skipping",
                                self, rmq_message.message_id.to_hex_string(), rmq_message_master_seqno, rmq_message.source_idx, old_status
                            );
                            #[cfg(feature = "telemetry")]
                            self.engine.remp_core_telemetry().add_to_cache_attempt(false);
                        }
                        Ok(None) => {
                            self.add_pending_collation(rmq_message, Some(RempMessageStatus::TonNode_RempNew)).await?;
                            #[cfg(feature = "telemetry")]
                            self.engine.remp_core_telemetry().add_to_cache_attempt(true);
                        }

                    }
                },
                Ok(Some(RmqRecord::TonNode_RmqMessageDigest(reject_digest))) => {
                    if MessageCache::cc_expired(reject_digest.masterchain_seqno as u32, self.catchain_info.master_cc_seqno) ||
                        reject_digest.masterchain_seqno as u32 > self.catchain_info.master_cc_seqno {
                        log::error!(target: "remp",
                            "Point 4. RMQ {}. Message digest (masterchain_seqno = {}, len = {}) is incompatible with current master seqno {}",
                            self, reject_digest.masterchain_seqno, reject_digest.messages.len(), self.catchain_info.master_cc_seqno
                        )
                    }
                    else {
                        log::info!(target: "remp",
                            "Point 4. RMQ {}. Message digest (masterchain_seqno = {}, len = {}) received",
                            self, reject_digest.masterchain_seqno, reject_digest.messages.len()
                        );
                        for message_id in reject_digest.messages.iter() {
                            let reject = RempRejected {
                                level: RempMessageLevel::TonNode_RempQueue,
                                block_id: Default::default(),
                                error: "reject received from digest".to_string()
                            };
                            self.remp_manager.message_cache.add_external_message_status(
                                &message_id, None, self.catchain_info.shard.clone(),
                                RempMessageStatus::TonNode_RempRejected(reject),
                                reject_digest.masterchain_seqno as u32
                            ).await?;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Prepare messages for collation - to be called just before collator invocation.
    pub async fn collect_messages_for_collation (&self) -> Result<()> {
        log::trace!(target: "remp", "RMQ {}: collecting messages for collation", self);
        let mut cnt = 0;
        while let Some((msgid, _timestamp)) = self.queues.execute_sync(|x| x.take_first_for_collation()).await? {
            let (status, message) = match self.remp_manager.message_cache.get_message_with_status(&msgid) {
                None => {
                    log::error!(
                        target: "remp",
                        "Point 5. RMQ {}: message {:x} found in pending_collation queue, but not in messages/message_statuses",
                        self, msgid
                    );
                    continue
                },
                Some((m, s)) => (s, m)
            };

            match status.clone() {
                RempMessageStatus::TonNode_RempNew 
              | RempMessageStatus::TonNode_RempIgnored(_) =>
                    log::trace!(target: "remp", "Point 5. RMQ {}: sending message {:x} to collator queue", self, msgid),
                _ => {
                    log::trace!(target: "remp", "Point 5. RMQ {}: Status for {:x} is too advanced to be sent to collator queue: {}",
                        self, msgid, status
                    );
                    continue
                }
            };

            match self.send_message_to_collator(msgid.clone(), message.message.clone()).await {
                Err(e) => {
                    let error = format!("{}", e);
                    log::error!(target: "remp",
                        "Point 5. RMQ {}: error sending message {:x} to collator: `{}`; message status is unknown till collation end",
                        self, msgid, &error
                    );

                    // No new status: failure inside collator does not say
                    // anything about the message. Let's wait till collation end.
                },
                Ok(()) => {
                    let new_status = RempMessageStatus::TonNode_RempAccepted (RempAccepted {
                        level: RempMessageLevel::TonNode_RempQueue,
                        block_id: BlockIdExt::default(),
                        master_id: BlockIdExt::default()
                    });
                    self.update_status_send_response(&msgid, message.clone(), new_status);
                    cnt = cnt + 1;
                }
            }
        }
        log::trace!(target: "remp", "Point 5. RMQ {}: total {} messages for collation", self, cnt);
        Ok(())
    }

    pub async fn all_accepted_by_collator_to_ignored(&self) -> Vec<(UInt256,u32)> {
        let mut downgrading = Vec::new();

        let to_check: Vec<(UInt256, bool)> = self.queues.execute_sync(|mq| {
            mq.pending_collation_set.iter().map(|(x,out_of_queue)| (x.clone(),*out_of_queue)).collect()
        }).await;

        // If message is not removed from collation queue --- and not passed to collator (or even further,
        // to shardchain/masterchain), it may not have status "accepted by collator"
        for (c, out_of_queue) in to_check {
            if let Some(ts) = self.remp_manager.message_cache.change_accepted_by_collator_to_ignored(&c) {
                downgrading.push((c.clone(), ts));
                if !out_of_queue {
                    log::error!(target: "remp",
                        "RMQ {}: message {:x} had status 'accepted by collator', however it is out_of_queue",
                        self, c
                    );
                }
            }
        }

        downgrading
    }

    /// Puts message to collation queue of the current collator.
    /// Each message queue has two stores: collation set (all messages, which are under collation) and
    /// collation queue (all messages, waiting for collator action).
    /// If the message is absent from the collation set, it is added there
    /// If the message is absent from the collation queue, it is added there
    /// If the message is present in the collation queue, it is skipped
    /// If the message is absent from message cache, an error is returned
    pub async fn add_to_collation_queue(&self, message_id: &UInt256, add_if_absent: bool) -> Result<()> {
        if let Some(message) = self.remp_manager.message_cache.get_message(message_id) {
            self.queues.execute_sync(
                |catchain| catchain.add_to_collation_queue(message_id, message.timestamp, add_if_absent)
            ).await?;
            Ok(())
        }
        else {
            fail!(
                "Point 6. RMQ {}: message {:x} is ignored by collator but absent, cannot return to collation queue",
                self, message_id
            )
        }
    }

    /// Processes one collator response from engine.deque_remp_message_status().
    /// Returns Ok(status) if the response was sucessfully processed, Ok(None) if no responses remain
    pub async fn process_one_deque_remp_message_status(&self) -> Result<Option<RempMessageStatus>> {
        let (collator_result, _pending) = self.remp_manager.collator_receipt_dispatcher.poll(&self.catchain_info.shard).await;

        match collator_result {
            Some(collator_result) => {
                log::trace!(target: "remp", "Point 6. REMP {} message {:x}, processing result {:?}",
                    self, collator_result.message_id, collator_result.status
                );
                let status = &collator_result.status;
                match status {
                    RempMessageStatus::TonNode_RempRejected(r) if r.level == RempMessageLevel::TonNode_RempCollator => {
                        self.update_status_send_response_by_id(&collator_result.message_id, status.clone()).await;
                    },
                    RempMessageStatus::TonNode_RempAccepted(a) if a.level == RempMessageLevel::TonNode_RempCollator => {
                        self.update_status_send_response_by_id(&collator_result.message_id, status.clone()).await;
                    },
                    RempMessageStatus::TonNode_RempIgnored(i) if i.level == RempMessageLevel::TonNode_RempCollator => {
                        // Part 2. All messages, ignored by collator itself are also a subject for re-collation
                        self.update_status_send_response_by_id(&collator_result.message_id, status.clone()).await;
                        log::trace!(target: "remp", 
                            "Point 6. RMQ {}: message {:x} ignored by collator, returning to collation queue", 
                            self, collator_result.message_id
                        );
                        self.add_to_collation_queue(&collator_result.message_id, false).await?;
                    },
                    _ => fail!(
                            "Point 6. RMQ {}: unexpected message status {} for RMQ message {:x}",
                            self, status, collator_result.message_id
                        )
                };
                Ok(Some(status.clone()))
            },
            None => Ok(None)
        }
    }

    /// Check message status after collation - to be called immediately after collator invocation.
    pub async fn process_collation_result (&self) {
        log::trace!(target: "remp", "Point 6. RMQ {}: processing collation results", self);
        let mut accepted = 0;
        let mut rejected = 0;
        let mut ignored = 0;

        loop {
            match self.process_one_deque_remp_message_status().await {
                Err(e) => {
                    log::error!(target: "remp",
                        "RMQ {}: cannot query message status, error {}",
                        self, e
                    );
                    break
                },
                Ok(None) => break,

                Ok(Some(RempMessageStatus::TonNode_RempAccepted(_))) => accepted += 1,
                Ok(Some(RempMessageStatus::TonNode_RempRejected(_))) => rejected += 1,
                Ok(Some(RempMessageStatus::TonNode_RempIgnored(_))) => ignored += 1,

                Ok(Some(s)) => {
                    log::error!(target: "remp", "Point 6. RMQ {}: unexpected status {}", self, s);
                    break
                }
            }
        }

        log::trace!(target: "remp", "Point 6. RMQ {}: total {} results processed (accepted {}, rejected {}, ignored {})", 
            self, accepted + rejected + ignored, accepted, rejected, ignored
        );
    }

    pub fn get_message(&self, id: &UInt256) -> Option<Arc<RmqMessage>> {
        self.remp_manager.message_cache.get_message(id)
    }

    pub async fn received_messages_count (&self) -> usize {
        self.queues.execute_sync(|mq| mq.pending_collation_set.len()).await
    }

    pub async fn print_messages (&self, name: &str, _count_only: bool) {
//      if count_only {
        log::debug!(target: "remp", "Queue {} (queue name {}), received message count {} (message printing not supported)",
            self, name, self.received_messages_count().await
        );
/*
        }

        else {
            let messages = self.received_messages_to_vector().await;
            for (msg,status) in &messages {
                log::debug!(target: "remp", "Queue {} (queue name {}), received message {}, status {}", 
                    self, name, msg, status
                );
            }
        }
 */
    }

    async fn send_message_to_collator(&self, message_id: UInt256, message: Arc<Message>) -> Result<()> {
        self.remp_manager.collator_receipt_dispatcher.queue.send_message_to_collator(
            message_id, message
        ).await
    }

    //pub fn pack_payload
}

impl Drop for MessageQueue {
    fn drop(&mut self) {
        log::trace!(target: "remp", "Dropping MessageQueue session {}", self);
    }
}

impl fmt::Display for MessageQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}*{:x}*{}*{}@{}",
               self.catchain_info.local_idx,
               self.catchain_info.queue_id,
               self.catchain_info.shard,
               self.catchain_info.master_cc_seqno,
               self.catchain_instance.get_id()
        )
    }
}

struct StatusUpdater {
    queue: Arc<MessageQueue>,
    new_status: RempMessageStatus
}

#[async_trait::async_trait]
impl BlockProcessor for StatusUpdater {
    fn process_message(&self, message_id: &UInt256) {
        match self.queue.get_message(message_id) {
            None => log::warn!(target: "remp", "Cannot find message {:x} in cache", message_id),
            Some(message) => {
                log::trace!(target: "remp", "Point 7. RMQ {} shard accepted message {}, new status {}", 
                    self.queue, message, self.new_status
                );
                self.queue.update_status_send_response(message_id, message, self.new_status.clone())
            }
        }
    }
}

/** Controls message queues - actual and next */
pub struct RmqQueueManager {
    engine: Arc<dyn EngineOperations>,
    remp_manager: Arc<RempManager>,
    shard: ShardIdent,
    cur_queue: Option<Arc<MessageQueue>>,
    next_queues: MutexWrapper<Vec<Arc<MessageQueue>>>,
    local_public_key: PublicKey
}

impl RmqQueueManager {
    pub fn new(
        engine: Arc<dyn EngineOperations>,
        remp_manager: Arc<RempManager>,
        shard: ShardIdent,
        local_public_key: &PublicKey
    ) -> Self {
        // If RMQ is not enabled, current & prev queues are not created.
        let manager = RmqQueueManager {
            engine,
            remp_manager,
            shard: shard.clone(),
            cur_queue: None,
            next_queues: MutexWrapper::new(vec!(), format!("Next queues for {}", shard.clone())),
            local_public_key: local_public_key.clone(),
            //status: MessageQueueStatus::Active
        };

        manager
    }

    pub fn set_queues(&mut self, catchain_seqno: u32, master_cc_seqno: u32, curr: &Vec<ValidatorDescr>, next: &Vec<ValidatorDescr>) {
        if self.remp_manager.options.is_service_enabled() {
            if self.cur_queue.is_some() {
                log::error!("RMQ Queue {}: attempt to re-initialize", self);
            }

            self.cur_queue = match MessageQueue::create(
                self.engine.clone(), self.remp_manager.clone(),
                self.shard.clone(), catchain_seqno, master_cc_seqno, curr, next, &self.local_public_key
            ) {
                Ok(t) => Some(Arc::new(t)),
                Err(error) => {
                    log::error!("Cannot create queue: {}", error);
                    None
                }
            }
        }
    }

    pub async fn add_new_queue(&self, next_queue_cc_seqno: u32, next_master_cc_seqno: u32, prev_validators: &Vec<ValidatorDescr>, next_validators: &Vec<ValidatorDescr>) {
        if !self.remp_manager.options.is_service_enabled() {
            return;
        }

        if let Some(_) = &self.cur_queue {
            //self.ensure_status(MessageQueueStatus::NewQueues)?;
            log::trace!(target: "remp", "RMQ {}: adding next queue {}", self, next_queue_cc_seqno);
            let queue = Arc::new(match MessageQueue::create(
                self.engine.clone(), self.remp_manager.clone(),
                self.shard.clone(), next_queue_cc_seqno, next_master_cc_seqno, 
                prev_validators, next_validators, &self.local_public_key
            ) {
                Ok(x) => x,
                Err(e) => {
                    log::error!(target: "remp", "RMQ {}: cannot create next queue, error `{}`", self, e);
                    return
                }
            });
            let added = self.next_queues.execute_sync(|q| {
                for one_queue in q.iter() {
                    if one_queue.catchain_info.is_same_catchain(queue.catchain_info.clone()) { 
                        return false; 
                    }
                }
                q.push(queue.clone());
                true
            }).await;

            if added {
                log::debug!(target: "remp", "RMQ {}: added next queue {}", self, queue);
            }
            else {
                log::trace!(target: "remp", "RMQ {}: next queue {} is already there; no addition", self, queue);
            }
        }
        else {
            log::error!(target: "remp", "RMQ {}: cannot add new queue: cur_queue is none!", self);
        }
    }

    pub async fn forward_messages(&self, new_cc_seqno: u32, local_key: PrivateKey) {
        if !self.remp_manager.options.is_service_enabled() {
            return;
        }

        log::info!(target: "remp", "RMQ {}: forwarding messages to new RMQ (cc_seqno {})", self, new_cc_seqno);
        let mut sent = 0;

        if let Some(queue) = &self.cur_queue {
            let to_forward = match queue.queues.execute_sync(|x| x.list_pending_for_forwarding()).await {
                Ok(f) => f,
                Err(e) => {
                    log::error!(
                        target: "remp",
                        "RMQ {}: cannot retrieve messages for forwarding to next REMP queue, error `{}`",
                        self, e
                    );
                    return
                }
            };

            log::info!(target: "remp", "RMQ {}: {} pending messages, starting next queues", self, to_forward.len());

            let new_queues = self.next_queues.execute_sync( |q| q.clone() ).await;

            if new_queues.len() == 0 {
                log::trace!(target: "remp", "RMQ {}: no next queues to forward messages", self);
                return
            }

            for q in new_queues.iter() {
                log::debug!(target: "remp", "RMQ {}: Starting {}", self, q);
                if let Err(e) = q.clone().start(local_key.clone()).await {
                    log::error!(
                        target: "remp",
                        "RMQ {}: cannot start next queue {}: `{}`",
                        self, q, e
                    );
                }
            }

            let mut rejected_message_digests: HashMap<u32, RmqMessageDigest> = HashMap::default();

            for msgid in to_forward.iter() {
                let (status, message, master_cc) = match self.remp_manager.message_cache.get_message_with_status_and_master_cc(msgid) {
                    None => {
                        log::error!(
                            target: "remp",
                            "Point 5a. RMQ {}: message {:x} found in pending_collation queue, but not in messages/message_statuses",
                            self, msgid
                        );
                        continue
                    },
                    Some((_m, RempMessageStatus::TonNode_RempTimeout, _master_cc)) => continue,
                    Some((_m, _s, master_cc)) if MessageCache::cc_expired(master_cc, new_cc_seqno) => continue,
                    Some((m, s, master_cc)) => (s, m, master_cc)
                };

                // Forwarding:
                // 1. All messages, originating from current session
                // 2. Finalized messages (accepted/rejected) -- as plain id (message id, is_accepted)
                // 3. Non-final messages -- (message body, is_forwarded = true)

                // Accepting of forwarded messages:
                // 1. If at least one message with body -- put it as "non-final"

                if is_finally_rejected(&status) {
                    if let Some(digest) = rejected_message_digests.get_mut (&master_cc) {
                        digest.messages.0.push(message.message_id.clone());
                    }
                    else {
                        let mut digest = ton_api::ton::ton_node::rmqrecord::RmqMessageDigest::default();
                        digest.masterchain_seqno = master_cc as i32;
                        digest.messages.0.push(message.message_id.clone());
                        rejected_message_digests.insert(master_cc, digest);
                    }
                }
                else if !is_finally_accepted(&status) {
                    if MessageQueue::is_final_status(&status) {
                        log::error!(target: "remp",
                            "Point 5a. RMQ {}: message {} status {} is final, but not finally accepted or rejected",
                            self, msgid, status
                        );
                    }

                    for new in new_queues.iter() {
                        if let Err(x) = new.catchain_instance.pending_messages_queue_send(message.as_rmq_record(master_cc)) {
                            log::error!(target: "remp",
                            "Point 5a. RMQ {}: message {:x} cannot be put to new queue {}: `{}`",
                            self, msgid, new, x
                        )
                        } else {
                            sent = sent + 1;
                        }
                    }
                }
            }

            for (master_cc, digest) in rejected_message_digests.into_iter() {
                if !digest.messages.0.is_empty() {
                    let digest_len = digest.messages.0.len();
                    let msg = ton_api::ton::ton_node::RmqRecord::TonNode_RmqMessageDigest(digest);
                    for new in new_queues.iter() {
                        if let Err(x) = new.catchain_instance.pending_messages_queue_send(msg.clone()) {
                            log::error!(target: "remp",
                            "Point 5a. RMQ {}: message digest (len={}) cannot be put to new queue {}: `{}`",
                            self, digest_len, new, x
                        )
                        } else {
                            sent = sent + 1;
                        }
                    }
                }
                else {
                    log::error!(target: "remp", "RMQ {}: rejected message digest for {} is empty, but present in cache!", self, master_cc)
                }
            }

            log::info!(target: "remp", "RMQ {}: forwarding messages to new RMQ, total {}, actually sent {}",
                self, to_forward.len(), sent
            );
        }
        else {
            log::warn!(target: "remp", "RMQ {}: cannot forward messages from non-existing queue", self);
        }
    }

    pub async fn start(&self, local_key: PrivateKey) -> Result<()> {
        if let Some(cur_queue) = &self.cur_queue {
            cur_queue.clone().start(local_key).await
        }
        else {
            log::warn!(target: "remp", "Cannot start RMQ queue for {} -- no current queue",
                self.info_string().await
            );
            Ok(())
        }
    }

    pub async fn stop(&self) {
        if let Some(q) = &self.cur_queue {
            if let Err(e) = q.stop().await {
                log::error!(target: "remp", "Cannot stop RMQ {} current queue {}: `{}`", self, q, e);
            }
        }
        while let Some(q) = self.next_queues.execute_sync (|q| q.pop()).await {
            if let Err(e) = q.stop().await {
                log::error!(target: "remp", "Cannot stop RMQ {} next queue {}: `{}`", self, q, e);
            }
        }
    }

    // pub fn make_test_message(&self) -> RmqMessage {
    //     return RmqMessage::make_test_message();
    // }

    pub async fn put_message_to_rmq(&self, message: Arc<RmqMessage>) -> Result<()> {
        if let Some(cur_queue) = &self.cur_queue {
            cur_queue.clone().put_message_to_rmq(message).await
        }
        else {
            fail!("Cannot put message to RMQ {}: queue is not started yet", self)
        }
    }

    pub async fn collect_messages_for_collation (&self) -> Result<()> {
        if let Some(queue) = &self.cur_queue {
            queue.collect_messages_for_collation().await?;
            Ok(())
        }
        else {
            fail!("Collecting messages for collation: RMQ {} is not started", self)
        }
    }

    pub async fn process_collation_result (&self) -> Result<()> {
        if let Some(queue) = &self.cur_queue {
            queue.process_collation_result().await;
            Ok(())
        }
        else {
            Err(err_msg(format!("Processing collation result: RMQ {} is not started", self)))
        }
    }

    pub async fn process_messages_from_accepted_shardblock(&self, id: BlockIdExt) -> Result<()> {
        if let Some(queue) = &self.cur_queue {
            let proc = Arc::new(StatusUpdater {
                queue: queue.clone(),
                new_status: RempMessageStatus::TonNode_RempAccepted(
                    ton_api::ton::ton_node::rempmessagestatus::RempAccepted {
                        level: RempMessageLevel::TonNode_RempShardchain,
                        block_id: id.clone(),
                        master_id: BlockIdExt::default(),
                    }
                )
            });
            process_block_messages_by_blockid(self.engine.clone(), id, proc).await?;

            // Point 7, Part 1. Collect and restart collation for all accepted by collator, but ignored in shardchain.
            let returned_msgs = queue.all_accepted_by_collator_to_ignored().await;
            for (msg_id, _timestamp) in returned_msgs.iter() {
                log::trace!(target: "remp", "Point 7. RMQ {} returning message {} to collation queue", self, msg_id);
                queue.add_to_collation_queue(msg_id, false).await?;
            };
            Ok(())
        }
        else {
            fail!("Applying message block {}: RMQ {} is not started", id, self)
        }
    }

    pub async fn poll(&self) {
        log::trace!(target: "remp", "Point 2. RMQ {} manager: polling incoming messages", self);
        if let Some(cur_queue) = &self.cur_queue {
            if !cur_queue.is_session_active() {
                log::warn!(target: "remp", "RMQ {} is not active yet, waiting...", self);
            }
            else if let Err(e) = cur_queue.poll().await {
                log::error!(target: "remp", "Error polling RMQ {} incoming messages: `{}`", self, e);
            }

            let mut cnt = 0;
            'a: loop {
                match self.remp_manager.poll_incoming(&self.shard).await {
                    (Some(rmq_message), _) => {
                        if let Err(e) = self.put_message_to_rmq(rmq_message.clone()).await {
                            log::warn!(target: "remp", "Error sending RMQ {} message {:?}: {}; returning back to incoming queue",
                                self, rmq_message, e
                            );
                            self.remp_manager.return_to_incoming(rmq_message, &self.shard).await;
                            break 'a;
                        }
                        else {
                            cnt = cnt+1;
                        }
                    }
                    (None, pending) => {
                        #[cfg(feature = "telemetry")]
                        self.engine.remp_core_telemetry().pending_from_fullnode(pending);
                        break 'a;
                    }
                }
            }
            #[cfg(feature = "telemetry")]
            self.engine.remp_core_telemetry().messages_from_fullnode_for_shard(&self.shard, cnt);

            log::trace!(target: "remp", "RMQ {} manager: finished polling incoming messages, {} messages processed", self, cnt);
        }
        else {
            log::warn!(target: "remp", "Cannot poll RMQ {}: current queue is not defined", self.info_string().await)
        }
    }

    pub async fn info_string(&self) -> String {
        if self.remp_manager.options.is_service_enabled() {
            format!("ReliableMessageQueue for shard {}: {}", self.shard, self)
        }
        else {
            format!("ReliableMessageQueue for shard {}: disabled", self.shard)
        }
    }

    pub async fn print_messages(&self, count_only: bool) {
        if self.remp_manager.options.is_service_enabled() {
            if let Some(cq) = &self.cur_queue { cq.print_messages("cur", count_only).await }
        }
        else {
            log::warn!(target: "remp", "ReliableMessageQueue for shard {}: disabled", self.shard);
        }
    }
}

impl Drop for RmqQueueManager {
    fn drop(&mut self) {
        log::trace!(target: "remp", "RMQ session {} dropped", self);
    }
}

impl fmt::Display for RmqQueueManager {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let cur = if let Some(q) = &self.cur_queue { format!("{}", q) } else { format!("*none*") };
/*
        let new = if self.new_queues.len() > 0 { 
            self.new_queues.iter().map( |q| format!(" {}",q) ).collect() 
        } else { 
            format!(" *none*")
        };
        write!(f, "{}, new:{}", cur, new)
 */
        write!(f, "{}", cur)
    }
}

