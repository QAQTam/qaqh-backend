//! Ringing daemon runtime state that sits beside the session owner:
//! command receipts, large content, leases, and device records.

#![cfg_attr(not(test), forbid(unsafe_code))]

pub mod attachment;
pub mod device_registry;
pub mod hub;
pub mod lease_store;
pub mod pending_store;

pub use attachment::hydrate_attachment_previews;
pub use device_registry::{DeviceRecord, DeviceRegistry, Scope};
pub use hub::RingingHub;
pub use lease_store::RingingLeaseStore;
pub use pending_store::{ExistingCommandReceipt, PendingCommandStore};
