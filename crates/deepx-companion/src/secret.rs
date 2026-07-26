pub fn generate_secret_hex() -> String {
    let bytes: [u8; 32] = rand::random();
    let mut output = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::generate_secret_hex;

    #[test]
    fn generates_distinct_256_bit_lower_hex_secrets() {
        let first = generate_secret_hex();
        let second = generate_secret_hex();
        assert_eq!(first.len(), 64);
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert_ne!(first, second);
    }
}
