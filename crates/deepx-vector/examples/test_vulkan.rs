//! Quick test for Vulkan GPU embedding.
//! Run: cargo run -p deepx-vector --features vulkan --example test_vulkan --release

use std::path::Path;
use std::time::Instant;

use deepx_vector::{Embedder, VulkanEmbedder};

fn main() {
    let path = Path::new(r"C:\Users\QAQTam\Downloads\Qwen3-Embedding-4B-Q4_K_M.gguf");
    if !path.exists() {
        eprintln!("GGUF not found: {:?}", path);
        return;
    }

    println!("Loading model (Vulkan GPU, 36 layers)...");
    let t0 = Instant::now();
    let embedder = match VulkanEmbedder::new(path, 36) {
        Ok(e) => e,
        Err(e) => { eprintln!("Load failed: {e}"); return; }
    };
    println!("Loaded in {:.2?}", t0.elapsed());

    for text in ["你好世界", "Rust language", "DeepX agent"] {
        let t0 = Instant::now();
        match embedder.embed(text) {
            Ok(v) => println!("  \"{text}\" → dim={}, {:.2?}", v.len(), t0.elapsed()),
            Err(e) => println!("  \"{text}\" → ERR: {e}"),
        }
    }
}
