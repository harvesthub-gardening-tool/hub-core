use crate::ble::MotorDispatchFailure;
use crate::config;
use crate::grpc::HubClient;
use anyhow::Result;
use log::{error, info, warn};
use protos_rust::control::v1::{
    MotorCommand, MotorCommandAction, MotorCommandReasonCode, MotorCommandStatus,
};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const POLLING_THREAD_STACK: usize = 32 * 1024;
const MOTOR_DISPATCH_RESPONSE_TIMEOUT_MS: u64 = 20_000;

pub(crate) struct MotorDispatchRequest {
    pub(crate) command_id: String,
    pub(crate) node_id: String,
    pub(crate) action: u8,
    pub(crate) duration_ms: i32,
    pub(crate) expires_at_epoch_ms: i64,
    pub(crate) response_tx: mpsc::Sender<Result<(), MotorDispatchFailure>>,
}

#[derive(Clone, Default)]
pub(crate) struct RadioMemoryGate {
    mutex: Arc<Mutex<()>>,
}

impl RadioMemoryGate {
    pub(crate) fn lock(&self) -> MutexGuard<'_, ()> {
        self.mutex.lock().expect("radio/memory gate mutex poisoned")
    }
}

pub fn spawn_command_polling_worker(
    hub_device_id: String,
    jwt: String,
    dispatch_tx: mpsc::SyncSender<MotorDispatchRequest>,
    radio_memory_gate: RadioMemoryGate,
) -> Result<()> {
    thread::Builder::new()
        .name("cmd-poll".into())
        .stack_size(POLLING_THREAD_STACK)
        .spawn(move || {
            if let Err(e) =
                run_command_polling_worker(hub_device_id, jwt, dispatch_tx, radio_memory_gate)
            {
                error!("command polling worker terminated: {e:#}");
            }
        })?;

    Ok(())
}

fn run_command_polling_worker(
    hub_device_id: String,
    jwt: String,
    dispatch_tx: mpsc::SyncSender<MotorDispatchRequest>,
    radio_memory_gate: RadioMemoryGate,
) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .max_blocking_threads(1)
        .build()?;

    rt.block_on(async move {
        let mut client: Option<HubClient> = None;
        let mut backoff_ms: u64 = config::MOTOR_COMMAND_POLL_BACKOFF_INITIAL_MS;
        let mut command_dedup = CommandDedupSet::default();

        loop {
            if client.is_none() {
                let connect_result = {
                    let _radio_memory_guard = radio_memory_gate.lock();
                    log_heap("before-command-poll-connect");
                    HubClient::connect_with_token(&jwt).await
                };

                match connect_result {
                    Ok(c) => {
                        info!("command polling gRPC client connected");
                        client = Some(c);
                        backoff_ms = config::MOTOR_COMMAND_POLL_BACKOFF_INITIAL_MS;
                    }
                    Err(e) => {
                        error!("command polling gRPC connect failed: {e:#}");
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        backoff_ms = (backoff_ms.saturating_mul(2))
                            .min(config::MOTOR_COMMAND_POLL_BACKOFF_MAX_MS);
                        continue;
                    }
                }
            }

            let pulled_result = {
                let _radio_memory_guard = radio_memory_gate.lock();
                log_heap("before-command-poll-rpc");
                client
                    .as_mut()
                    .expect("client exists")
                    .pull_pending_motor_commands(
                        &hub_device_id,
                        config::MOTOR_COMMAND_POLL_BATCH_SIZE,
                        config::MOTOR_COMMAND_POLL_LEASE_DURATION_MS,
                    )
                    .await
            };

            let pulled = match pulled_result {
                Ok(commands) => {
                    backoff_ms = config::MOTOR_COMMAND_POLL_BACKOFF_INITIAL_MS;
                    commands
                }
                Err(e) => {
                    error!("command polling RPC failed: {e:#}");
                    client = None;
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms.saturating_mul(2))
                        .min(config::MOTOR_COMMAND_POLL_BACKOFF_MAX_MS);
                    continue;
                }
            };

            if !pulled.is_empty() {
                info!("command polling received {} command(s)", pulled.len());
            }

            for command in pulled {
                handle_polled_command(
                    &hub_device_id,
                    client.as_mut().expect("client exists"),
                    &dispatch_tx,
                    &mut command_dedup,
                    &radio_memory_gate,
                    command,
                )
                .await;
            }

            let interval_ms = poll_interval_with_jitter_ms();
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
        }
    });

    Ok(())
}

async fn handle_polled_command(
    hub_device_id: &str,
    client: &mut HubClient,
    dispatch_tx: &mpsc::SyncSender<MotorDispatchRequest>,
    command_dedup: &mut CommandDedupSet,
    radio_memory_gate: &RadioMemoryGate,
    command: MotorCommand,
) {
    let command_id = command.command_id.clone();
    let node_id = command.node_id.clone();

    match command_dedup.try_begin(&command_id) {
        DedupBegin::AlreadyProcessed => {
            info!(
                "skipping duplicate polled command already terminal in-memory: command_id={} node_id={}",
                command_id, node_id
            );
            return;
        }
        DedupBegin::AlreadyProcessing => {
            info!(
                "skipping duplicate polled command already in-flight in-memory: command_id={} node_id={}",
                command_id, node_id
            );
            return;
        }
        DedupBegin::New => {}
    };

    if command_is_expired(&command) {
        warn!(
            "polled expired command ignored: command_id={} node_id={} status={} reason_code={} expires_at={}",
            command_id,
            node_id,
            MotorCommandStatus::Expired.as_str_name(),
            MotorCommandReasonCode::Expired.as_str_name(),
            command.expires_at
        );
        ack_best_effort(
            client,
            radio_memory_gate,
            &command_id,
            hub_device_id,
            &node_id,
            MotorCommandStatus::Expired,
            MotorCommandReasonCode::Expired,
            "command expired before dispatch",
        )
        .await;
        command_dedup.finish_processed(&command_id);
        return;
    }

    let action = match parse_command_action(&command) {
        ParsedCommandAction::RunForDuration { duration_ms } => {
            if duration_ms < 0 {
                warn!(
                    "polled command rejected before dispatch: command_id={} node_id={} status={} reason_code={} detail=invalid_negative_duration duration_ms={}",
                    command_id,
                    node_id,
                    MotorCommandStatus::Failed.as_str_name(),
                    MotorCommandReasonCode::BleWriteFailed.as_str_name(),
                    duration_ms,
                );
                ack_best_effort(
                    client,
                    radio_memory_gate,
                    &command_id,
                    hub_device_id,
                    &node_id,
                    MotorCommandStatus::Failed,
                    MotorCommandReasonCode::BleWriteFailed,
                    "invalid negative motor duration",
                )
                .await;
                command_dedup.finish_processed(&command_id);
                return;
            }
            config::MOTOR_COMMAND_ACTION_RUN_FOR_DURATION
        }
        ParsedCommandAction::Stop => config::MOTOR_COMMAND_ACTION_STOP,
        ParsedCommandAction::Unknown { raw_action } => {
            warn!(
                "polled command rejected before dispatch: command_id={} node_id={} status={} reason_code={} detail=unsupported_action raw_action={}",
                command_id,
                node_id,
                MotorCommandStatus::Failed.as_str_name(),
                MotorCommandReasonCode::BleWriteFailed.as_str_name(),
                raw_action,
            );
            ack_best_effort(
                client,
                radio_memory_gate,
                &command_id,
                hub_device_id,
                &node_id,
                MotorCommandStatus::Failed,
                MotorCommandReasonCode::BleWriteFailed,
                &format!("unsupported motor action value: {raw_action}"),
            )
            .await;
            command_dedup.finish_processed(&command_id);
            return;
        }
    };

    let (response_tx, response_rx) = mpsc::channel::<Result<(), MotorDispatchFailure>>();
    let request = MotorDispatchRequest {
        command_id: command_id.clone(),
        node_id: node_id.clone(),
        action,
        duration_ms: command.duration_ms,
        expires_at_epoch_ms: command.expires_at,
        response_tx,
    };

    if let Err(e) = dispatch_tx.send(request) {
        error!(
            "motor dispatch enqueue failed: command_id={} node_id={} status={} reason_code={} error={}",
            command_id,
            node_id,
            MotorCommandStatus::Failed.as_str_name(),
            MotorCommandReasonCode::BleWriteFailed.as_str_name(),
            e,
        );
        ack_best_effort(
            client,
            radio_memory_gate,
            &command_id,
            hub_device_id,
            &node_id,
            MotorCommandStatus::Failed,
            MotorCommandReasonCode::BleWriteFailed,
            &format!("motor dispatch channel unavailable: {e}"),
        )
        .await;
        command_dedup.finish_processed(&command_id);
        return;
    }

    match response_rx.recv_timeout(Duration::from_millis(MOTOR_DISPATCH_RESPONSE_TIMEOUT_MS)) {
        Ok(Ok(())) => {
            info!(
                "motor dispatch completed: command_id={} node_id={} dispatch_status={} ack_status={} reason_code={} ack_result=queued",
                command_id,
                node_id,
                "probe_write_acknowledged",
                MotorCommandStatus::SentToProbe.as_str_name(),
                MotorCommandReasonCode::None.as_str_name(),
            );
            ack_best_effort(
                client,
                radio_memory_gate,
                &command_id,
                hub_device_id,
                &node_id,
                MotorCommandStatus::SentToProbe,
                MotorCommandReasonCode::None,
                "motor command write acknowledged by probe",
            )
            .await;
            command_dedup.finish_processed(&command_id);
        }
        Ok(Err(MotorDispatchFailure::ProbeUnreachable)) => {
            warn!(
                "motor dispatch failed: command_id={} node_id={} dispatch_status=probe_unreachable ack_status={} reason_code={} ack_result=queued",
                command_id,
                node_id,
                MotorCommandStatus::Failed.as_str_name(),
                MotorCommandReasonCode::ProbeUnreachable.as_str_name(),
            );
            ack_best_effort(
                client,
                radio_memory_gate,
                &command_id,
                hub_device_id,
                &node_id,
                MotorCommandStatus::Failed,
                MotorCommandReasonCode::ProbeUnreachable,
                "target probe not found over BLE",
            )
            .await;
            command_dedup.finish_processed(&command_id);
        }
        Ok(Err(MotorDispatchFailure::Expired)) => {
            warn!(
                "motor dispatch expired before BLE write: command_id={} node_id={} dispatch_status=expired ack_status={} reason_code={} ack_result=queued",
                command_id,
                node_id,
                MotorCommandStatus::Expired.as_str_name(),
                MotorCommandReasonCode::Expired.as_str_name(),
            );
            ack_best_effort(
                client,
                radio_memory_gate,
                &command_id,
                hub_device_id,
                &node_id,
                MotorCommandStatus::Expired,
                MotorCommandReasonCode::Expired,
                "command expired before BLE write",
            )
            .await;
            command_dedup.finish_processed(&command_id);
        }
        Ok(Err(MotorDispatchFailure::BleWriteFailed(message))) => {
            warn!(
                "motor dispatch BLE failure: command_id={} node_id={} dispatch_status=ble_write_failed ack_status={} reason_code={} detail={}",
                command_id,
                node_id,
                MotorCommandStatus::Failed.as_str_name(),
                MotorCommandReasonCode::BleWriteFailed.as_str_name(),
                message,
            );
            ack_best_effort(
                client,
                radio_memory_gate,
                &command_id,
                hub_device_id,
                &node_id,
                MotorCommandStatus::Failed,
                MotorCommandReasonCode::BleWriteFailed,
                &message,
            )
            .await;
            command_dedup.finish_processed(&command_id);
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            warn!(
                "motor dispatch response timed out: command_id={} node_id={} dispatch_status=dispatcher_timeout ack_status={} reason_code={} ack_result=queued",
                command_id,
                node_id,
                MotorCommandStatus::Failed.as_str_name(),
                MotorCommandReasonCode::BleWriteFailed.as_str_name(),
            );
            ack_best_effort(
                client,
                radio_memory_gate,
                &command_id,
                hub_device_id,
                &node_id,
                MotorCommandStatus::Failed,
                MotorCommandReasonCode::BleWriteFailed,
                "timed out waiting for BLE dispatch result",
            )
            .await;
            command_dedup.finish_processed(&command_id);
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            warn!(
                "motor dispatch responder disconnected: command_id={} node_id={} dispatch_status=dispatcher_disconnected ack_status={} reason_code={} ack_result=queued",
                command_id,
                node_id,
                MotorCommandStatus::Failed.as_str_name(),
                MotorCommandReasonCode::BleWriteFailed.as_str_name(),
            );
            ack_best_effort(
                client,
                radio_memory_gate,
                &command_id,
                hub_device_id,
                &node_id,
                MotorCommandStatus::Failed,
                MotorCommandReasonCode::BleWriteFailed,
                "BLE dispatch responder disconnected",
            )
            .await;
            command_dedup.finish_processed(&command_id);
        }
    }
}

fn parse_command_action(command: &MotorCommand) -> ParsedCommandAction {
    match MotorCommandAction::try_from(command.action) {
        Ok(MotorCommandAction::RunForDuration) => ParsedCommandAction::RunForDuration {
            duration_ms: command.duration_ms,
        },
        Ok(MotorCommandAction::Stop) => ParsedCommandAction::Stop,
        Ok(MotorCommandAction::Unspecified) | Err(_) => ParsedCommandAction::Unknown {
            raw_action: command.action,
        },
    }
}

enum ParsedCommandAction {
    RunForDuration { duration_ms: i32 },
    Stop,
    Unknown { raw_action: i32 },
}

async fn ack_best_effort(
    client: &mut HubClient,
    radio_memory_gate: &RadioMemoryGate,
    command_id: &str,
    hub_id: &str,
    node_id: &str,
    status: MotorCommandStatus,
    reason_code: MotorCommandReasonCode,
    reason_message: &str,
) {
    let ack_result = {
        let _radio_memory_guard = radio_memory_gate.lock();
        log_heap("before-command-ack-rpc");
        client
            .ack_motor_command_event(
                command_id,
                hub_id,
                node_id,
                status,
                reason_code,
                reason_message,
            )
            .await
    };

    if let Err(e) = ack_result {
        error!(
            "command ack failed: command_id={} node_id={} status={} reason_code={} error={e:#}",
            command_id,
            node_id,
            status.as_str_name(),
            reason_code.as_str_name(),
        );
    } else {
        info!(
            "command ack sent: command_id={} node_id={} status={} reason_code={} reason_message={}",
            command_id,
            node_id,
            status.as_str_name(),
            reason_code.as_str_name(),
            reason_message,
        );
    }
}

fn log_heap(label: &str) {
    let free_heap = unsafe { esp_idf_svc::sys::esp_get_free_heap_size() };
    let min_free_heap = unsafe { esp_idf_svc::sys::esp_get_minimum_free_heap_size() };
    info!("[HEAP] {label}: free={free_heap} min_free={min_free_heap}");
}

#[derive(Default)]
struct CommandDedupSet {
    processing: HashSet<String>,
    processed: HashMap<String, i64>,
}

enum DedupBegin {
    New,
    AlreadyProcessing,
    AlreadyProcessed,
}

impl CommandDedupSet {
    fn try_begin(&mut self, command_id: &str) -> DedupBegin {
        self.prune();

        if self.processed.contains_key(command_id) {
            return DedupBegin::AlreadyProcessed;
        }

        if self.processing.contains(command_id) {
            return DedupBegin::AlreadyProcessing;
        }

        self.processing.insert(command_id.to_string());
        DedupBegin::New
    }

    fn prune(&mut self) {
        let now = unix_now_ms();
        let retention = config::MOTOR_COMMAND_DUPLICATE_RETENTION_MS as i64;
        self.processed
            .retain(|_, seen_at| now.saturating_sub(*seen_at) <= retention);
    }

    fn finish_processed(&mut self, command_id: &str) {
        self.processing.remove(command_id);
        self.processed.insert(command_id.to_string(), unix_now_ms());

        self.prune();
    }
}

fn command_is_expired(command: &MotorCommand) -> bool {
    let now_ms = unix_now_ms();

    command.expires_at > 0 && command.expires_at <= now_ms
}

fn unix_now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(ts) => ts.as_millis() as i64,
        Err(_) => 0,
    }
}

fn poll_interval_with_jitter_ms() -> u64 {
    let base = config::MOTOR_COMMAND_POLL_INTERVAL_MS;
    let jitter = config::MOTOR_COMMAND_POLL_JITTER_MS;

    if jitter == 0 {
        return base;
    }

    let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(ts) => ts.subsec_nanos() as u64,
        Err(_) => 0,
    };

    let span = jitter.saturating_mul(2).saturating_add(1);
    let offset = (now % span) as i64 - jitter as i64;
    base.saturating_add_signed(offset)
}
