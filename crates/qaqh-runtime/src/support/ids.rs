//! Wire → canonical id derivation shared by the thin session owner.

use sha2::Digest;

pub(crate) fn ulid_from_text(text: &str) -> String {
    let mut hash = sha2::Sha256::digest(text.as_bytes());
    hash[0] = 0x01;
    hash[1] = 0x4a;
    let mut encoded = String::with_capacity(26);
    const CROCKFORD_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    for i in 0..26 {
        let byte = hash[i % hash.len()];
        encoded.push(CROCKFORD_ALPHABET[(byte % 32) as usize] as char);
    }
    encoded
}

pub(crate) fn canonical_interaction_id(
    wire_interaction_id: &str,
) -> qaqh_session::session_fact_v2::InteractionId {
    qaqh_session::session_fact_v2::InteractionId::new(format!(
        "int_{}",
        ulid_from_text(wire_interaction_id)
    ))
}
