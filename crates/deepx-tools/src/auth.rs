//! PIN / password authentication for sensitive tool operations.
//!
//! On all platforms, prompts the user via the terminal (stderr) for a PIN,
//! then compares the SHA-256 hash against a stored `pin_token` file.
//!
//! The `pin_token` file is stored in `<data_dir>/pin_token` and contains
//! the hex-encoded SHA-256 of the expected PIN.

use sha2::Digest;
use std::path::PathBuf;

/// Verify the user's identity by prompting for a PIN / password.
/// Returns `true` if authentication succeeds.
pub fn verify_pin(reason: &str) -> bool {
    log::info!("auth: requesting PIN for reason: {reason}");
    let path = pin_token_path();
    let stored = match std::fs::read_to_string(&path) {
        Ok(s) => s.trim().to_string(),
        Err(_) => {
            log::warn!("auth: no pin_token file at {}, auth denied", path.display());
            return false;
        }
    };

    // Prompt for PIN via stderr (terminal prompt)
    eprint!("[AUTH] {} Enter PIN: ", reason);
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    let input = input.trim().to_string();
    let input_hash = {
        let mut hasher = sha2::Sha256::new();
        hasher.update(input.as_bytes());
        hex::encode(hasher.finalize())
    };

    if input_hash == stored {
        log::info!("auth: PIN verification succeeded");
        true
    } else {
        log::warn!("auth: PIN verification failed");
        false
    }
}

/// Generate a short-lived session auth token (for internal use).
pub fn session_auth_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let raw = format!("deepx-session-{ts}-{}", std::process::id());
    let hash = sha2::Sha256::digest(raw.as_bytes());
    hex::encode(hash)
}

fn pin_token_path() -> PathBuf {
    deepx_types::platform::data_dir().join("pin_token")
}
