//! Read-only system queries and RingingV2 global configuration mutations.

use serde_json::{Value, json};

use qaqh_ringing::RingingCommand;

use crate::error::{QaqhError, QaqhResult};
use crate::service::QaqhService;
use crate::service::command::SystemCommand;
use crate::service::stats::load_config;

pub(crate) fn handle(service: &QaqhService, command: SystemCommand) -> QaqhResult<Value> {
    match command {
        SystemCommand::DaemonVersion(_) => Ok(json!(env!("QAQH_BUILD_VERSION"))),
        SystemCommand::DiagnosticsMemorySnapshot(args) => service
            .diagnostics_memory_snapshot(args.after_sequence)
            .map_err(QaqhError::message),
        SystemCommand::ConfigLoad(_) => load_config().map_err(QaqhError::message),
    }
}

pub(crate) fn handle_ringing(service: &QaqhService, command: &RingingCommand) -> QaqhResult<Value> {
    match command {
        RingingCommand::ConfigSave { patch } => {
            let patch: qaqh_config_api::ConfigPatch = serde_json::from_value(patch.clone())
                .map_err(|error| {
                    QaqhError::invalid(format!("invalid config.save payload: {error}"))
                })?;
            service.save_config(patch)?;
            load_config().map_err(QaqhError::message)
        }
        RingingCommand::ConfigSetPermissionLevel { level } => {
            if !(1..=4).contains(level) {
                return Err(QaqhError::invalid(format!(
                    "invalid permission level {level} (must be 1-4: 1=read-only, 2=workspace-write, 3=skip-permissions, 4=os-sandbox)"
                )));
            }
            service.update_config_and_reload(|cfg| {
                cfg.permission_level = *level;
                Ok(())
            })?;
            log::info!("[config] permission level set to {level}");
            Ok(json!({ "permission_level": level }))
        }
        RingingCommand::DiagnosticsMemoryStart {} => {
            let monitor = qaqh_memwatch::global();
            monitor.start();
            service.refresh_memory_components();
            Ok(json!({
                "enabled": true,
                "started_at_ms": monitor.started_at_ms()
            }))
        }
        RingingCommand::DiagnosticsMemoryStop {} => {
            qaqh_memwatch::global().stop();
            Ok(json!({ "enabled": false }))
        }
        _ => Err(QaqhError::invalid(format!(
            "{} is not a global configuration command",
            command.name()
        ))),
    }
}

