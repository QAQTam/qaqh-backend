use qaqh_ringing::RingingV2SyncResult;
use qaqh_ringing_v3::{ControlReply, ErrorCode, RpcError};

use super::RealBackend;

const OPERATION: &str = "ringing.v3.control";

fn storage_key(device: &str, command_id: &str) -> String {
    format!("v3:{device}:{command_id}")
}

pub(crate) async fn lookup(
    backend: &RealBackend,
    device: &str,
    command_id: &str,
    fingerprint: &str,
) -> Result<Option<ControlReply>, RpcError> {
    let receipts = std::sync::Arc::clone(&backend.receipts);
    let key = storage_key(device, command_id);
    let fingerprint = fingerprint.to_string();
    let owner = device.to_string();
    RealBackend::blocking(move || {
        let receipts = receipts.lock().unwrap_or_else(|error| error.into_inner());
        let Some(status) = receipts.v2_status_for_session(&key, &owner) else {
            return Ok(None);
        };
        if status.payload_fingerprint != fingerprint {
            return Err(RpcError {
                code: ErrorCode::Conflict,
                message: "command_id 已存在且请求指纹不符".to_string(),
                retryable: false,
            });
        }
        let Some(sync) = status.sync_result else {
            return Err(RpcError {
                code: ErrorCode::Unavailable,
                message: "同一 command_id 的上一请求结果未知，拒绝重复执行".to_string(),
                retryable: false,
            });
        };
        if sync.operation != OPERATION {
            return Err(RpcError {
                code: ErrorCode::Conflict,
                message: "command_id 已被其他协议占用".to_string(),
                retryable: false,
            });
        }
        serde_json::from_value(sync.result)
            .map(Some)
            .map_err(|error| RpcError {
                code: ErrorCode::Internal,
                message: format!("持久回执损坏：{error}"),
                retryable: false,
            })
    })
    .await
}

pub(crate) async fn reserve(
    backend: &RealBackend,
    device: &str,
    command_id: &str,
    fingerprint: &str,
) -> Result<(), RpcError> {
    let receipts = std::sync::Arc::clone(&backend.receipts);
    let key = storage_key(device, command_id);
    let fingerprint = fingerprint.to_string();
    let owner = device.to_string();
    RealBackend::blocking(move || {
        match receipts
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .record_fingerprint_for_session(&key, &fingerprint, &owner)
        {
            Ok(true) => Ok(()),
            Ok(false) => Err(RpcError {
                code: ErrorCode::Unavailable,
                message: "同一 command_id 的上一请求结果未知，拒绝重复执行".to_string(),
                retryable: false,
            }),
            Err(()) => Err(RpcError {
                code: ErrorCode::Conflict,
                message: "command_id 已存在且请求指纹不符".to_string(),
                retryable: false,
            }),
        }
    })
    .await
}

pub(crate) async fn persist(
    backend: &RealBackend,
    device: &str,
    command_id: &str,
    reply: &ControlReply,
) -> Result<(), RpcError> {
    let receipts = std::sync::Arc::clone(&backend.receipts);
    let key = storage_key(device, command_id);
    let owner = device.to_string();
    let reply = reply.clone();
    RealBackend::blocking(move || {
        let result = serde_json::to_value(&reply).map_err(|error| RpcError {
            code: ErrorCode::Internal,
            message: format!("序列化 V3 回执失败：{error}"),
            retryable: false,
        })?;
        let mut receipts = receipts.lock().unwrap_or_else(|error| error.into_inner());
        receipts.mark_terminal_sync(
            &key,
            RingingV2SyncResult {
                operation: OPERATION.to_string(),
                result,
                revision: None,
            },
        );
        let persisted = receipts
            .v2_status_for_session(&key, &owner)
            .and_then(|status| status.sync_result)
            .filter(|sync| sync.operation == OPERATION);
        if persisted.is_none() {
            return Err(RpcError {
                code: ErrorCode::Internal,
                message: "V3 回执落库校验失败".to_string(),
                retryable: false,
            });
        }
        Ok(())
    })
    .await
}
