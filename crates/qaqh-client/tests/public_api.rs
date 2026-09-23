//! Public API compile checks for native shells.
//!
//! Shells must be able to name every value they construct through the
//! `qaqh-client` root without reaching into `qaqh-domain` directly.

use qaqh_client::ConversationInputPurpose;

#[test]
fn conversation_input_purpose_is_named_at_client_root() {
    let purpose: ConversationInputPurpose = Default::default();
    assert_eq!(purpose, ConversationInputPurpose::TriggerTurn);
}
