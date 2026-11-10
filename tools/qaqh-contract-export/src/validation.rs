//! Artifact integrity and import-closure checks; no TypeScript toolchain required.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path};

use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize, serde::Serialize)]
struct FileHash {
    path: String,
    sha256: String,
}

#[derive(Deserialize)]
struct Manifest {
    files: Vec<FileHash>,
    contract_hash: String,
}

pub(super) fn validate(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let paths = super::list_files(root)?;
    let manifest: Manifest = serde_json::from_slice(&fs::read(root.join("manifest.json"))?)?;
    let declared: Vec<_> = manifest
        .files
        .iter()
        .map(|item| item.path.clone())
        .collect();
    let actual: Vec<_> = paths
        .iter()
        .filter(|path| path.as_str() != "manifest.json")
        .cloned()
        .collect();
    if declared != actual {
        return Err("manifest file set is missing, unsorted, duplicated or stale".into());
    }
    for entry in &manifest.files {
        let bytes = fs::read(root.join(&entry.path))?;
        if entry.sha256 != hex::encode(Sha256::digest(bytes)) {
            return Err(format!("manifest digest mismatch: {}", entry.path).into());
        }
    }
    if manifest.contract_hash != hex::encode(Sha256::digest(serde_json::to_vec(&manifest.files)?)) {
        return Err("manifest contract_hash mismatch".into());
    }
    let all: BTreeSet<_> = paths.iter().cloned().collect();
    for path in paths.iter().filter(|path| path.ends_with(".ts")) {
        let source = fs::read_to_string(root.join(path))?;
        // ts-rs emits one-line import/export clauses with quoted module names.
        for line in source.lines().filter(|line| {
            line.trim_start().starts_with("import ") || line.trim_start().starts_with("export ")
        }) {
            let Some((_, module)) = line.split_once(" from \"") else {
                continue;
            };
            let Some((module, _)) = module.split_once('"') else {
                return Err(format!("unterminated TypeScript module import: {path}").into());
            };
            if !module.starts_with('.') {
                return Err(
                    format!("external dependency in generated contract: {path}: {module}").into(),
                );
            }
            let mut relative = Path::new(path)
                .parent()
                .unwrap_or(Path::new(""))
                .to_path_buf();
            for component in Path::new(module).components() {
                match component {
                    Component::CurDir => {}
                    Component::Normal(part) => relative.push(part),
                    Component::ParentDir if relative.pop() => {}
                    _ => {
                        return Err(
                            format!("module escapes contract directory: {path}: {module}").into(),
                        );
                    }
                }
            }
            let name = format!("{}.ts", relative.to_string_lossy().replace('\\', "/"));
            if !all.contains(&name) {
                return Err(format!("unresolved TypeScript import: {path}: {module}").into());
            }
        }
    }
    let public_index = fs::read_to_string(root.join("index.ts"))?;
    if public_index.contains("DaemonDiscovery") {
        return Err("host credential DTO must only be exported from node.ts".into());
    }
    // coverage.json 声明的 fixture 必须真实存在于产物里：声明与文件集的
    // 漂移（如命名替换不一致）会让消费者按图索骥落空，却没有任何门禁红。
    let coverage: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("coverage.json"))?)?;
    let empty: Vec<serde_json::Value> = Vec::new();
    // B02 路由漂移闸(双向)。coverage 声明展开后的 (method, path) 全集:
    // 正向(声明 ⊆ 表)挡虚报,反向(表 ⊆ 声明)挡漏报 —— 后者让消费者按
    // coverage.json 找不到端点,而 contract_hash 完全不变(ADR-0003 §缺口 3)。
    let mut declared: BTreeSet<(String, String)> = BTreeSet::new();
    for capability in coverage["capabilities"].as_array().unwrap_or(&empty) {
        let id = capability["id"].as_str().unwrap_or_default();
        for fixture in capability["fixtures"].as_array().unwrap_or(&empty) {
            let Some(name) = fixture.as_str() else {
                continue;
            };
            let relative = format!("fixtures/{name}");
            if !all.contains(&relative) {
                return Err(format!(
                    "coverage capability {id} declares missing fixture: {relative}"
                )
                .into());
            }
        }
        // 复合声明("GET + POST"、"/a/b + /events")以前被整条跳过,等于虚报闸
        // 对这些能力失效;现在展开后逐对校验。
        let method = capability["method"].as_str().unwrap_or_default();
        let path = capability["path"].as_str().unwrap_or_default();
        for (route_method, route_path) in
            expand_routes(method, path).map_err(|problem| format!("{id}: {problem}"))?
        {
            let known = qaqh_ringing::RINGING_V2_HTTP_ROUTES
                .iter()
                .any(|(known_method, known_path)| {
                    *known_method == route_method && *known_path == route_path
                });
            if !known {
                return Err(format!(
                    "coverage capability {id} declares route {route_method} {route_path} that is not in RINGING_V2_HTTP_ROUTES"
                )
                .into());
            }
            declared.insert((route_method, route_path));
        }
    }
    // 反向:路由表里每条 (method, path) 都必须被某个能力声明,或显式登记在
    // UNCOVERED_ROUTES 里并附理由。新增路由却忘了写能力记录,从此不可能静默通过。
    for (route_method, route_path) in qaqh_ringing::RINGING_V2_HTTP_ROUTES {
        let pair = ((*route_method).to_owned(), (*route_path).to_owned());
        if declared.contains(&pair)
            || UNCOVERED_ROUTES
                .iter()
                .any(|(verb, route)| *verb == pair.0 && *route == pair.1)
        {
            continue;
        }
        return Err(format!(
            "RINGING_V2_HTTP_ROUTES declares {route_method} {route_path} that no coverage capability documents; add it to coverage.rs or to UNCOVERED_ROUTES with a reason"
        )
        .into());
    }
    Ok(())
}

/// 端点已实现、但按 `coverage.json` 的能力粒度还没写记录的情形。每一项都必须
/// 成文理由;清空它比留着它好(见 coverage.rs 里 pending 能力的同款纪律)。
const UNCOVERED_ROUTES: [(&str, &str); 0] = [];

/// 把一条能力的 `method` / `path` 声明展开成 `(method, path)` 序列。
///
/// - 非路由声明(如 `file` / `daemon.json`)返回空,不参与本闸;
/// - `+` 分隔方法与路径;相对段(`"/events"`)按前一个绝对段的目录展开;
/// - `/ringing/v2/service/{具体方法}` 归一到表里的 `/ringing/v2/service/{method}`;
/// - 方法数与路径数既不是一对一、也不是一多 ⇒ 判定为含糊声明并报错(fail-closed),
///   绝不自作聪明地猜组合。
fn expand_routes(method: &str, path: &str) -> Result<Vec<(String, String)>, String> {
    let split_terms = |text: &str| -> Vec<String> {
        text.split('+')
            .map(str::trim)
            .filter(|term| !term.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let mut paths: Vec<String> = Vec::new();
    let mut last_absolute: Option<String> = None;
    for term in split_terms(path) {
        if term.starts_with("/ringing/v2/") {
            // `/ringing/v2/service/{具体方法}` 归一到表里的参数化字面量。
            paths.push(match term.strip_prefix("/ringing/v2/service/") {
                Some(_) => "/ringing/v2/service/{method}".to_owned(),
                None => term.clone(),
            });
            last_absolute = Some(term);
        } else if let Some(base) = &last_absolute {
            let Some((directory, _)) = base.rsplit_once('/') else {
                return Err(format!(
                    "relative segment {term:?} cannot be expanded against base {base:?}"
                ));
            };
            paths.push(format!("{directory}{term}"));
        } else {
            // 非 Ringing 路由声明(如 control 的 `file` / `daemon.json`)交由
            // daemon 的 CONTROL_PLANE_ROUTES 与 snapshots/routes.txt 冻结。
            return Ok(Vec::new());
        }
    }
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let methods = split_terms(method);
    if methods.is_empty() {
        return Err(format!("route path {path:?} is declared without any method"));
    }
    // 一对一、或一对多;其它组合一律判为含糊声明,不自作聪明猜笛卡尔积。
    let pairs: Vec<(String, String)> = if methods.len() == 1 {
        let verb = methods[0].clone();
        paths.into_iter().map(|route| (verb.clone(), route)).collect()
    } else if paths.len() == 1 {
        let route = paths[0].clone();
        methods.into_iter().map(|verb| (verb, route.clone())).collect()
    } else if methods.len() == paths.len() {
        methods.into_iter().zip(paths).collect()
    } else {
        return Err(format!(
            "ambiguous route declaration: {} methods vs {} paths (method={method:?}, path={path:?})",
            methods.len(),
            paths.len()
        ));
    };
    Ok(pairs)
}
