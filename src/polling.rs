use crate::ble::MotorDispatchFailure;
use crate::config;
use crate::grpc::HubClient;
use anyhow::Result;
use log::{error, info, warn};
use protos_rust::control::v1::{
    MotorCommand, MotorCommandAction, MotorCommandReasonCode, MotorCommandStatus,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) struct MotorDispatchRequest {
    pub(crate) command_id: String,
    pub(crate) action: u8,
    pub(crate) duration_ms: i32,
    pub(crate) expires_at_epoch_ms: i64,
}

struct PendingMotorCommand {
    command_id: String,
    node_id: String,
    action: u8,
    duration_ms: i32,
    expires_at_epoch_ms: i64,
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

#[derive(Default)]
pub(crate) struct CommandPoller {
    command_dedup: CommandDedupSet,
    pending: Vec<PendingMotorCommand>,
}

impl CommandPoller {
    pub(crate) fn poll_once(
        &mut self,
        hub_device_id: &str,
        jwt: &str,
        radio_memory_gate: &RadioMemoryGate,
    ) {
        self.expire_pending_commands(hub_device_id, jwt, radio_memory_gate);

        let pulled = match pull_commands_once(jwt, hub_device_id, radio_memory_gate) {
            Ok(commands) => commands,
            Err(e) => {
                error!("command polling connect/RPC failed: {e:#}");
                return;
            }
        };

        if !pulled.is_empty() {
            info!("command polling received {} command(s)", pulled.len());
        }

        for command in pulled {
            self.store_polled_command(hub_device_id, jwt, radio_memory_gate, command);
        }
    }

    pub(crate) fn peek_pending_for_probe_uuid(
        &self,
        probe_uuid: &str,
    ) -> Option<MotorDispatchRequest> {
        self.pending
            .iter()
            .find(|command| command.node_id == probe_uuid)
            .map(|command| MotorDispatchRequest {
                command_id: command.command_id.clone(),
                action: command.action,
                duration_ms: command.duration_ms,
                expires_at_epoch_ms: command.expires_at_epoch_ms,
            })
    }

    pub(crate) fn complete_dispatched_for_probe(
        &mut self,
        hub_device_id: &str,
        jwt: &str,
        radio_memory_gate: &RadioMemoryGate,
        probe_uuid: &str,
        dispatch_result: std::result::Result<(), MotorDispatchFailure>,
    ) {
        let mut remaining = Vec::with_capacity(self.pending.len());
        let pending = core::mem::take(&mut self.pending);
        let mut consumed = false;

        for command in pending {
            if consumed || command.node_id != probe_uuid {
                remaining.push(command);
                continue;
            }

            consumed = true;
            if let Some(command) = self.dispatch_pending_command(
                hub_device_id,
                jwt,
                radio_memory_gate,
                command,
                &mut |_| dispatch_result.clone(),
            ) {
                remaining.push(command);
            }
        }

        self.pending = remaining;
    }

    fn store_polled_command(
        &mut self,
        hub_device_id: &str,
        jwt: &str,
        radio_memory_gate: &RadioMemoryGate,
        command: MotorCommand,
    ) {
        let command_id = command.command_id.clone();
        let node_id = command.node_id.clone();

        match self.command_dedup.try_begin(&command_id) {
            DedupBegin::AlreadyProcessed => {
                info!(
                    "skipping duplicate polled command already terminal in-memory: command_id={} node_id={}",
                    command_id, node_id
                );
                return;
            }
            DedupBegin::AlreadyProcessing => {
                info!(
                    "skipping duplicate polled command already pending in-memory: command_id={} node_id={}",
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
                jwt,
                radio_memory_gate,
                &command_id,
                hub_device_id,
                &node_id,
                MotorCommandStatus::Expired,
                MotorCommandReasonCode::Expired,
                "command expired before dispatch",
            );
            self.command_dedup.finish_processed(&command_id);
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
                        jwt,
                        radio_memory_gate,
                        &command_id,
                        hub_device_id,
                        &node_id,
                        MotorCommandStatus::Failed,
                        MotorCommandReasonCode::BleWriteFailed,
                        "invalid negative motor duration",
                    );
                    self.command_dedup.finish_processed(&command_id);
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
                    jwt,
                    radio_memory_gate,
                    &command_id,
                    hub_device_id,
                    &node_id,
                    MotorCommandStatus::Failed,
                    MotorCommandReasonCode::BleWriteFailed,
                    &format!("unsupported motor action value: {raw_action}"),
                );
                self.command_dedup.finish_processed(&command_id);
                return;
            }
        };

        info!(
            "motor command pending until probe availability: command_id={} node_id={} expires_at={}",
            command_id, node_id, command.expires_at
        );
        self.pending.push(PendingMotorCommand {
            command_id,
            node_id,
            action,
            duration_ms: command.duration_ms,
            expires_at_epoch_ms: command.expires_at,
        });
    }

    fn dispatch_pending_command(
        &mut self,
        hub_device_id: &str,
        jwt: &str,
        radio_memory_gate: &RadioMemoryGate,
        command: PendingMotorCommand,
        dispatch_motor: &mut impl FnMut(
            MotorDispatchRequest,
        ) -> std::result::Result<(), MotorDispatchFailure>,
    ) -> Option<PendingMotorCommand> {
        let request = MotorDispatchRequest {
            command_id: command.command_id.clone(),
            action: command.action,
            duration_ms: command.duration_ms,
            expires_at_epoch_ms: command.expires_at_epoch_ms,
        };

        match dispatch_motor(request) {
            Ok(()) => {
                info!(
                    "motor dispatch completed: command_id={} node_id={} dispatch_status={} ack_status={} reason_code={} ack_result=queued",
                    command.command_id,
                    command.node_id,
                    "probe_write_acknowledged",
                    MotorCommandStatus::SentToProbe.as_str_name(),
                    MotorCommandReasonCode::None.as_str_name(),
                );
                let ack_sent = ack_best_effort(
                    jwt,
                    radio_memory_gate,
                    &command.command_id,
                    hub_device_id,
                    &command.node_id,
                    MotorCommandStatus::SentToProbe,
                    MotorCommandReasonCode::None,
                    "motor command write acknowledged by probe",
                );
                if ack_sent {
                    self.command_dedup.finish_processed(&command.command_id);
                    None
                } else {
                    warn!(
                        "motor dispatch ack failed, keeping command retryable until TTL expiry: command_id={} node_id={} expires_at={}",
                        command.command_id,
                        command.node_id,
                        command.expires_at_epoch_ms,
                    );
                    Some(command)
                }
            }
            Err(MotorDispatchFailure::Expired) => {
                warn!(
                    "motor dispatch expired before BLE write: command_id={} node_id={} dispatch_status=expired ack_status={} reason_code={} ack_result=queued",
                    command.command_id,
                    command.node_id,
                    MotorCommandStatus::Expired.as_str_name(),
                    MotorCommandReasonCode::Expired.as_str_name(),
                );
                ack_best_effort(
                    jwt,
                    radio_memory_gate,
                    &command.command_id,
                    hub_device_id,
                    &command.node_id,
                    MotorCommandStatus::Expired,
                    MotorCommandReasonCode::Expired,
                    "command expired before BLE write",
                );
                self.command_dedup.finish_processed(&command.command_id);
                None
            }
            Err(MotorDispatchFailure::BleWriteFailed(message)) => {
                warn!(
                    "motor dispatch BLE failure, keeping pending until TTL expiry: command_id={} node_id={} dispatch_status=ble_write_failed reason_code={} expires_at={} detail={}",
                    command.command_id,
                    command.node_id,
                    MotorCommandReasonCode::BleWriteFailed.as_str_name(),
                    command.expires_at_epoch_ms,
                    message,
                );
                Some(command)
            }
        }
    }

    fn expire_pending_commands(
        &mut self,
        hub_device_id: &str,
        jwt: &str,
        radio_memory_gate: &RadioMemoryGate,
    ) {
        let pending = core::mem::take(&mut self.pending);
        for command in pending {
            if pending_command_is_expired(&command) {
                warn!(
                    "pending motor command expired: command_id={} node_id={} status={} reason_code={} expires_at={}",
                    command.command_id,
                    command.node_id,
                    MotorCommandStatus::Expired.as_str_name(),
                    MotorCommandReasonCode::Expired.as_str_name(),
                    command.expires_at_epoch_ms,
                );
                ack_best_effort(
                    jwt,
                    radio_memory_gate,
                    &command.command_id,
                    hub_device_id,
                    &command.node_id,
                    MotorCommandStatus::Expired,
                    MotorCommandReasonCode::Expired,
                    "command expired before probe became available",
                );
                self.command_dedup.finish_processed(&command.command_id);
            } else {
                self.pending.push(command);
            }
        }
    }
}

fn pull_commands_once(
    jwt: &str,
    hub_device_id: &str,
    radio_memory_gate: &RadioMemoryGate,
) -> Result<Vec<MotorCommand>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .max_blocking_threads(1)
        .build()?;

    rt.block_on(async {
        let _radio_memory_guard = radio_memory_gate.lock();
        log_heap("before-command-poll-connect");
        let mut client = HubClient::connect_with_token(jwt).await?;

        info!("command polling gRPC client connected");
        log_heap("before-command-poll-rpc");
        client
            .pull_pending_motor_commands(
                hub_device_id,
                config::MOTOR_COMMAND_POLL_BATCH_SIZE,
                config::MOTOR_COMMAND_POLL_LEASE_DURATION_MS,
            )
            .await
    })
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

fn ack_best_effort(
    jwt: &str,
    radio_memory_gate: &RadioMemoryGate,
    command_id: &str,
    hub_id: &str,
    node_id: &str,
    status: MotorCommandStatus,
    reason_code: MotorCommandReasonCode,
    reason_message: &str,
) -> bool {
    let ack_result = ack_once(
        jwt,
        radio_memory_gate,
        command_id,
        hub_id,
        node_id,
        status,
        reason_code,
        reason_message,
    );

    if let Err(e) = ack_result {
        error!(
            "command ack failed: command_id={} node_id={} status={} reason_code={} error={e:#}",
            command_id,
            node_id,
            status.as_str_name(),
            reason_code.as_str_name(),
        );
        false
    } else {
        info!(
            "command ack sent: command_id={} node_id={} status={} reason_code={} reason_message={}",
            command_id,
            node_id,
            status.as_str_name(),
            reason_code.as_str_name(),
            reason_message,
        );
        true
    }
}

fn ack_once(
    jwt: &str,
    radio_memory_gate: &RadioMemoryGate,
    command_id: &str,
    hub_id: &str,
    node_id: &str,
    status: MotorCommandStatus,
    reason_code: MotorCommandReasonCode,
    reason_message: &str,
) -> Result<Option<MotorCommand>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .max_blocking_threads(1)
        .build()?;

    rt.block_on(async {
        let _radio_memory_guard = radio_memory_gate.lock();
        log_heap("before-command-ack-connect");
        let mut client = HubClient::connect_with_token(jwt).await?;

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
    })
}

fn log_heap(label: &str) {
    let free_heap = unsafe { esp_idf_svc::sys::esp_get_free_heap_size() };
    let min_free_heap = unsafe { esp_idf_svc::sys::esp_get_minimum_free_heap_size() };
    let free_8bit =
        unsafe { esp_idf_svc::sys::heap_caps_get_free_size(esp_idf_svc::sys::MALLOC_CAP_8BIT) };
    let largest_8bit = unsafe {
        esp_idf_svc::sys::heap_caps_get_largest_free_block(esp_idf_svc::sys::MALLOC_CAP_8BIT)
    };
    let internal_caps = esp_idf_svc::sys::MALLOC_CAP_INTERNAL | esp_idf_svc::sys::MALLOC_CAP_8BIT;
    let free_internal = unsafe { esp_idf_svc::sys::heap_caps_get_free_size(internal_caps) };
    let largest_internal =
        unsafe { esp_idf_svc::sys::heap_caps_get_largest_free_block(internal_caps) };
    info!(
        "[HEAP] {label}: free={free_heap} min_free={min_free_heap} free_8bit={free_8bit} largest_8bit={largest_8bit} free_internal={free_internal} largest_internal={largest_internal}"
    );
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

fn pending_command_is_expired(command: &PendingMotorCommand) -> bool {
    let now_ms = unix_now_ms();

    command.expires_at_epoch_ms > 0 && command.expires_at_epoch_ms <= now_ms
}

fn unix_now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(ts) => ts.as_millis() as i64,
        Err(_) => 0,
    }
}
