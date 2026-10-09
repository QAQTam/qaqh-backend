//! Lightweight, allocation-free heap-size estimates for runtime diagnostics.
//!
//! These values are intentionally estimates: allocator metadata, hash-table
//! buckets, shared backing storage, and fragmentation are not observable here.

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryUsageEstimate {
    /// Sum of owned string/value payload lengths (not wire JSON length).
    pub payload_bytes: u64,
    /// Textual content subset of `payload_bytes`.
    pub text_bytes: u64,
    /// Inline image bytes subset of `payload_bytes`.
    pub image_bytes: u64,
    /// Approximate heap size using capacities plus inline object storage.
    pub heap_estimate_bytes: u64,
    pub item_count: u64,
}

impl MemoryUsageEstimate {
    pub fn add(&mut self, other: Self) {
        self.payload_bytes = self.payload_bytes.saturating_add(other.payload_bytes);
        self.text_bytes = self.text_bytes.saturating_add(other.text_bytes);
        self.image_bytes = self.image_bytes.saturating_add(other.image_bytes);
        self.heap_estimate_bytes = self
            .heap_estimate_bytes
            .saturating_add(other.heap_estimate_bytes);
        self.item_count = self.item_count.saturating_add(other.item_count);
    }

    pub fn add_string(&mut self, value: &String, is_text: bool) {
        self.payload_bytes = self.payload_bytes.saturating_add(value.len() as u64);
        self.heap_estimate_bytes = self
            .heap_estimate_bytes
            .saturating_add(value.capacity() as u64);
        if is_text {
            self.text_bytes = self.text_bytes.saturating_add(value.len() as u64);
        }
    }

    pub fn add_image_string(&mut self, value: &String) {
        self.payload_bytes = self.payload_bytes.saturating_add(value.len() as u64);
        self.image_bytes = self.image_bytes.saturating_add(value.len() as u64);
        self.heap_estimate_bytes = self
            .heap_estimate_bytes
            .saturating_add(value.capacity() as u64);
    }

    pub fn add_vec_capacity<T>(&mut self, capacity: usize) {
        self.heap_estimate_bytes = self
            .heap_estimate_bytes
            .saturating_add((capacity.saturating_mul(std::mem::size_of::<T>())) as u64);
    }

    pub fn add_value(&mut self, value: &serde_json::Value) {
        self.heap_estimate_bytes = self
            .heap_estimate_bytes
            .saturating_add(std::mem::size_of::<serde_json::Value>() as u64);
        match value {
            serde_json::Value::String(value) => self.add_string(value, false),
            serde_json::Value::Array(values) => {
                self.add_vec_capacity::<serde_json::Value>(values.capacity());
                for value in values {
                    self.add_value(value);
                }
            }
            serde_json::Value::Object(values) => {
                for (key, value) in values {
                    self.add_string(key, false);
                    self.add_value(value);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }
}
