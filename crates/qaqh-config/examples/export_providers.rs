//! 一次性导出工具：把 registry 内置 13 个 provider 序列化为 assets/providers.toml，
//! 并做 round-trip 验证（parse 回来的表与内存表逐字段一致）。
//! 用法：cargo run -p qaqh-config --example export_providers

use qaqh_types::ProviderSpec;

#[derive(serde::Serialize)]
struct Doc<'a> {
    providers: &'a [ProviderSpec],
}

#[derive(serde::Deserialize)]
struct File {
    #[serde(default)]
    providers: Vec<ProviderSpec>,
}

fn main() {
    let providers: Vec<ProviderSpec> = qaqh_config::registry::all_providers();
    let doc = Doc {
        providers: &providers,
    };
    let mut out = String::from(
        "# QAQ-Harness 内置 provider/endpoint 能力基线（T9）。\n\
         # 本文件由 registry.rs 内置表导出生成（example export_providers）；运行时按\n\
         # override > config.toml > assets 优先级合并。\n\
         # 字段缺省 = OpenAI 纯协议语义（见 docs/current/architecture.md）。\n\n",
    );
    out.push_str(&toml::to_string_pretty(&doc).expect("serialize providers"));
    std::fs::write("assets/providers.toml", &out).expect("write providers.toml");

    // Round-trip 验证：parse 回来再序列化，逐 provider 逐字段比对。
    let raw = std::fs::read_to_string("assets/providers.toml").expect("read back");
    let file: File = toml::from_str(&raw).expect("parse back");
    assert_eq!(
        file.providers.len(),
        providers.len(),
        "provider count mismatch"
    );
    for (a, b) in providers.iter().zip(file.providers.iter()) {
        let sa = toml::to_string_pretty(&Doc {
            providers: std::slice::from_ref(a),
        })
        .expect("serialize provider");
        let sb = toml::to_string_pretty(&Doc {
            providers: std::slice::from_ref(b),
        })
        .expect("serialize provider");
        assert_eq!(sa, sb, "round-trip mismatch for provider {}", a.id);
    }
    println!(
        "exported + verified {} providers, {} bytes",
        providers.len(),
        out.len()
    );
}
