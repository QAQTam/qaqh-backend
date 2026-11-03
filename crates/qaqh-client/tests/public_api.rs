//! Public API compile checks for native shells.
//!
//! Shells must be able to name every value they construct through the
//! `qaqh-client` root without reaching into `qaqh-domain` directly.

use qaqh_client::{
    CommandPrecondition, CommandTarget, InputPurpose, RingingCommand,
    RingingV2CommandEnvelope, SessionSettingsPatch,
};

#[test]
fn ringing_input_purpose_is_named_at_client_root() {
    let purpose: InputPurpose = Default::default();
    assert_eq!(purpose, InputPurpose::TriggerTurn);
}

#[test]
fn settings_command_uses_the_single_ringing_envelope() {
    let envelope = RingingV2CommandEnvelope::new(
        "cmd-settings",
        CommandTarget::Session {
            session_id: "ses_public_api".into(),
        },
        RingingCommand::SessionSettingsPatch {
            patch: SessionSettingsPatch {
                profile: Some("reviewer".into()),
                ..Default::default()
            },
        },
    )
    .with_client_identity("instance-public-api", "lease-public-api")
    .with_precondition(CommandPrecondition::SettingsRevision { revision: 4 });
    assert!(envelope.validate().is_ok());
}
