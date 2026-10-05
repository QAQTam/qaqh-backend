//! 沙箱策略与 spawn 前 ACL 计划(纯函数,可单测)。
//!
//! JSON 形状是冻结契约(与 qaqh-policy::SandboxSpec 的映射在合入时做,
//! 方向永远是 "qaqh 适配 sbx",见 README 宪法第 2 条)。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    Deny,
    Allow,
}

impl NetworkPolicy {
    fn deny() -> Self {
        NetworkPolicy::Deny
    }
}

/// 隔离后端(ADR-0002):
/// - `token`:WRITE_RESTRICTED 受限令牌 + cap-SID DACL(只滤写,读透传)
/// - `appcontainer`:AppContainer 令牌,全部访问过 AC-SID 检查(读可隔离),
///   空 capability = 内核断网
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum IsolationKind {
    /// 单词形态与冻结契约一致(HANDOFF §2:"token" | "appcontainer")
    #[serde(rename = "token")]
    #[default]
    Token,
    /// 单词形态与 CLI --isolation 一致(不走 snake_case 的 app_container)
    #[serde(rename = "appcontainer")]
    AppContainer,
}

/// 沙箱策略(spec §4.1 经 cross-review 修订后的形状)。
///
/// - `writable_roots`:自动放行目录(整棵子树,构建类目录)
/// - `writable_files`:逐文件放行(write_paths 审批产物;不存在则预创建)
/// - `deny_write_paths`:写拒绝 carveout(如工作区 `.git`;deny 优先于 allow)
/// - `readable_roots`:appcontainer 后端的读放行子树(token 后端读永不隔离,本字段忽略)
/// - `isolation`:隔离后端(默认 token;契约向后兼容,省略即 token)
/// - `capabilities`:appcontainer 后端的 capability 授权(well-known 名或原始
///   S-1-15-3-* SID;空列表 = 内核断网。token 后端忽略——该字段只增权,不削弱隔离)
/// - `writable_registry_keys`:注册表键写授权(HKCU/HKLM/HKCR/HKU 缩写均接受,
///   不存在则预创建;未授权键的注册表写本就 fail-closed——restricting/AC 检查无 ACE)
/// - `redirect`:M2 重定向语义(spec §4.2 step 4 的 M2 分支,ADR-0003)。
///   true 时工作区内的授权目标重定向进 ProjFS 视图(store = 工作区只读基底,
///   turn 边界 diff → merge/discard),工作区外维持就地放行;需要 workspace
///   支持,仅 token 后端。缺省 false(纯平面一,向后兼容)
///
/// TokenPlane 读永不隔离(spec N2);appcontainer 后端读也过 AC 检查。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SbxPolicy {
    #[serde(default)]
    pub writable_roots: Vec<PathBuf>,
    #[serde(default)]
    pub writable_files: Vec<PathBuf>,
    #[serde(default)]
    pub deny_write_paths: Vec<PathBuf>,
    #[serde(default = "NetworkPolicy::deny")]
    pub network: NetworkPolicy,
    #[serde(default)]
    pub isolation: IsolationKind,
    #[serde(default)]
    pub readable_roots: Vec<PathBuf>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub writable_registry_keys: Vec<String>,
    #[serde(default)]
    pub redirect: bool,
}

/// 授权/拒绝目标必须先存在再挂 ACE(codex spawn_prep 教训:防
/// "沙箱子进程先在可写父下创建目标再继承父 ACL" 的 TOCTOU)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precreate {
    None,
    File,
    Dir,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AceMode {
    /// 写放行(WRITE_MASK)
    Allow,
    /// 写拒绝 carveout(WRITE_MASK 上显式 deny)
    Deny,
    /// 读+执行放行(READ_MASK;appcontainer 后端的 readable_roots)
    Read,
}

/// ACE 授权目标种类(文件与注册表共用同一条 apply/快照/台账流水线)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AceTarget {
    /// 文件系统对象(SE_FILE_OBJECT;apply 前做 reparse 组件扫描)
    File,
    /// 注册表键(SE_REGISTRY_OBJECT;路径形如 "HKCU\Software\x" 或全称)
    Registry,
}

/// 一条 spawn 前 ACL 操作(纯数据;由 [`crate::acl::apply_ops`] 执行)。
#[derive(Debug, Clone)]
pub struct AceOp {
    pub path: PathBuf,
    pub mode: AceMode,
    pub sid: Vec<u8>,
    /// true = 容器+对象继承(目录/子树);false = 仅本对象(叶子文件)
    pub inherit_tree: bool,
    pub precreate: Precreate,
    /// appcontainer 后端写授权需 FULL 级掩码(26300 内核对窄掩码的既有文件
    /// 打开仍拒绝;实证见 ADR-0002)。token 后端恒为 false。
    pub full_mask: bool,
    /// 授权目标种类(注册表键走 SE_REGISTRY_OBJECT 同构流水线)
    pub kind: AceTarget,
}

/// 由策略推导 spawn 前 ACL 计划。
///
/// deny 先于 allow 入列仅是可读性安排;实际优先级由系统保证
/// (SetEntriesInAclW 将 deny ACE 排在 DACL 前部,deny 语义恒胜)。
pub fn build_ace_plan(policy: &SbxPolicy, sid: &[u8]) -> Vec<AceOp> {
    let mut ops = Vec::new();
    // FULL 掩码只对 appcontainer 后端的写授权目标有意义(token 后端窄掩码即可)
    let full_mask = policy.isolation == IsolationKind::AppContainer;
    for p in &policy.deny_write_paths {
        ops.push(AceOp {
            path: p.clone(),
            mode: AceMode::Deny,
            sid: sid.to_vec(),
            inherit_tree: true,
            precreate: Precreate::Dir,
            full_mask: false,
            kind: AceTarget::File,
        });
    }
    for p in &policy.writable_roots {
        ops.push(AceOp {
            path: p.clone(),
            mode: AceMode::Allow,
            sid: sid.to_vec(),
            inherit_tree: true,
            precreate: Precreate::Dir,
            full_mask,
            kind: AceTarget::File,
        });
    }
    for p in &policy.writable_files {
        ops.push(AceOp {
            path: p.clone(),
            mode: AceMode::Allow,
            sid: sid.to_vec(),
            inherit_tree: false,
            precreate: Precreate::File,
            full_mask,
            kind: AceTarget::File,
        });
    }
    for p in &policy.readable_roots {
        ops.push(AceOp {
            path: p.clone(),
            mode: AceMode::Read,
            sid: sid.to_vec(),
            inherit_tree: true,
            precreate: Precreate::None,
            full_mask: false,
            kind: AceTarget::File,
        });
    }
    for p in &policy.writable_registry_keys {
        ops.push(AceOp {
            path: PathBuf::from(p),
            mode: AceMode::Allow,
            sid: sid.to_vec(),
            // 注册表子键继承(SUB_CONTAINERS_AND_OBJECTS_INHERIT 同义)
            inherit_tree: true,
            // 键不存在则预创建(与 writable_files 的预创建语义同构)
            precreate: Precreate::Dir,
            full_mask,
            kind: AceTarget::Registry,
        });
    }
    ops
}

impl AceMode {
    /// 该模式对应的必需访问掩码(幂等判定与授权掩码共用一处定义)
    pub fn required_mask(self) -> u32 {
        match self {
            AceMode::Allow | AceMode::Deny => crate::acl::WRITE_MASK,
            AceMode::Read => crate::acl::READ_MASK,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_shape() {
        let policy = SbxPolicy {
            writable_roots: vec![PathBuf::from("C:\\ws\\target")],
            writable_files: vec![PathBuf::from("C:\\ws\\a.txt")],
            deny_write_paths: vec![PathBuf::from("C:\\ws\\.git")],
            network: NetworkPolicy::Deny,
            isolation: IsolationKind::Token,
            readable_roots: vec![PathBuf::from("C:\\sdk")],
            capabilities: vec![],
            writable_registry_keys: vec!["HKCU\\Software\\sbx".into()],
            redirect: false,
        };
        let ops = build_ace_plan(&policy, &[1, 2]);
        assert_eq!(ops.len(), 5);
        assert_eq!(ops[0].mode, AceMode::Deny);
        assert_eq!(ops[0].inherit_tree, true);
        assert_eq!(ops[0].precreate, Precreate::Dir);
        assert_eq!(ops[1].mode, AceMode::Allow);
        assert_eq!(ops[2].inherit_tree, false);
        assert_eq!(ops[2].precreate, Precreate::File);
        assert_eq!(ops[3].mode, AceMode::Read);
        assert_eq!(ops[3].precreate, Precreate::None);
        assert!(!ops[1].full_mask, "token 后端窄掩码即可");
        // 注册表 op:Allow + 子键继承 + 预创建
        assert_eq!(ops[4].kind, AceTarget::Registry);
        assert_eq!(ops[4].mode, AceMode::Allow);
        assert_eq!(ops[4].inherit_tree, true);
        assert_eq!(ops[4].precreate, Precreate::Dir);
        let mut policy_ac = policy.clone();
        policy_ac.isolation = IsolationKind::AppContainer;
        for op in build_ace_plan(&policy_ac, &[1, 2])
            .iter()
            .filter(|o| o.mode == AceMode::Allow)
        {
            assert!(op.full_mask, "appcontainer 后端 Allow 目标必须用 FULL 掩码");
        }
    }

    #[test]
    fn json_contract() {
        let json = r#"{
            "writable_roots": ["C:\\ws\\target"],
            "writable_files": ["C:\\ws\\a.txt"],
            "deny_write_paths": ["C:\\ws\\.git"],
            "network": "deny"
        }"#;
        let p: SbxPolicy = serde_json::from_str(json).unwrap();
        assert_eq!(p.writable_roots.len(), 1);
        assert_eq!(p.isolation, IsolationKind::Token, "缺省隔离 = token(向后兼容)");

        // 全部字段可省
        let p2: SbxPolicy = serde_json::from_str("{}").unwrap();
        assert_eq!(p2.network, NetworkPolicy::Deny);
        assert!(p2.writable_roots.is_empty());
        assert!(p2.readable_roots.is_empty());
        assert!(p2.capabilities.is_empty());
        assert!(p2.writable_registry_keys.is_empty());
        assert!(!p2.redirect, "缺省关闭重定向(向后兼容)");

        // appcontainer + readable_roots + capabilities
        let p3: SbxPolicy = serde_json::from_str(
            r#"{"isolation":"appcontainer","readable_roots":["C:\\sdk"],"network":"deny","capabilities":["internetClient","S-1-15-3-2"]}"#,
        )
        .unwrap();
        assert_eq!(p3.isolation, IsolationKind::AppContainer);
        assert_eq!(p3.readable_roots.len(), 1);
        assert_eq!(p3.capabilities, vec!["internetClient", "S-1-15-3-2"]);

        // writable_registry_keys(缩写与全称都接受原样入库,展开在 acl 层做)
        let p4: SbxPolicy = serde_json::from_str(
            r#"{"writable_registry_keys":["HKCU\\Software\\sbx","HKEY_CURRENT_USER\\Software\\x"]}"#,
        )
        .unwrap();
        assert_eq!(p4.writable_registry_keys.len(), 2);
    }
}
