//! M2(三)RedirectPlane 接线 —— SbxPolicy 的重定向语义(spec §4.2 step 4 的
//! M2 分支,ADR-0003 裁决的落地)。
//!
//! 模型:store = 工作区(只读基底),视图 = 沙箱进程的工作面。策略路径按
//! "是否位于工作区内"分流:
//! - 工作区内 → **重定向进视图**:ACE 挂在视图侧对象上(占位继承视图
//!   DACL),视图内写落 upper,turn 边界 diff → 审批流 → merge/discard;
//! - 工作区外 → **就地放行**:平面一原样(scratch、外部目录、注册表),
//!   行为与 M1 完全一致。
//!
//! 子进程对真实工作区永远无 ACE(视图模式不给工作区挂任何 cap ACE),即使
//! 它通过环境推断出真实路径并直接打开,受限令牌检查也无 ACE 可匹配 → 内核
//! 拒绝。强制力仍在平面一;视图只是让"授权的写"可控可审(双平面分工)。
//!
//! Provider 生命周期 = run 进程内 turn(随 sbx run 起、随 turn 止)。spike
//! S7/S8 实证父进程服务跨进程水合成立,独立 daemon 进程无增量收益(决策
//! 记录见 HANDOFF v3);若未来跨 run 复用视图,再引入常驻进程不迟。

use std::io;
use std::path::Path;

use crate::acl;
use crate::policy::{AceMode, AceOp, AceTarget, Precreate, SbxPolicy};
use crate::projfs;

/// 视图相对路径(反斜杠形态,如 "sub\\a.txt";根 = "")。
#[derive(Debug, Clone)]
pub struct ViewOp {
    pub op: AceOp,
    pub rel: String,
}

/// 拆分结果:进视图的 ops(path 已重写为视图内绝对路径)与就地 ops。
#[derive(Debug, Clone, Default)]
pub struct SplitPlan {
    pub view: Vec<ViewOp>,
    pub inplace: Vec<AceOp>,
}

/// 路径是否位于 workspace 之下;是则返回规范化相对路径(反斜杠形态,
/// 工作区本身 = "")。规范化经 sbx_nt::normalize(小写/反斜杠/无前缀),
/// 大小写差异天然消除;8.3 短名等不可解析形态返回 None → 走就地流水线,
/// 由平面一的路径无关强制兜底。
pub fn workspace_rel(workspace: &Path, p: &Path) -> Option<String> {
    let ws = sbx_nt::normalize(workspace.to_string_lossy().as_ref()).ok()?;
    let np = sbx_nt::normalize(p.to_string_lossy().as_ref()).ok()?;
    let rel = if np == ws {
        String::new()
    } else {
        let prefix = format!("{ws}\\");
        np.strip_prefix(&prefix)?.to_string()
    };
    Some(rel)
}

/// 由策略推导视图/就地拆分计划(复用 [`crate::policy::build_ace_plan`],
/// 保证两种模式对同一策略生成同一套授权语义)。
pub fn split_plan(
    policy: &SbxPolicy,
    sid: &[u8],
    workspace: &Path,
    view_root: &Path,
) -> io::Result<SplitPlan> {
    let mut out = SplitPlan::default();
    for mut op in crate::policy::build_ace_plan(policy, sid) {
        if op.kind != AceTarget::File {
            out.inplace.push(op);
            continue;
        }
        match workspace_rel(workspace, &op.path) {
            Some(rel) => {
                let mut view_path = view_root.to_path_buf();
                if !rel.is_empty() {
                    view_path.push(rel.replace('/', "\\"));
                }
                op.path = view_path;
                // 视图侧写授权恒 FULL 掩码:copy-up/tombstone/内核水合的打开
                // 要求超出窄掩码(ADR-0002 事实 2 同理;spike S7 以 FULL 实证)
                if op.mode == AceMode::Allow {
                    op.full_mask = true;
                }
                out.view.push(ViewOp { op, rel });
            }
            None => out.inplace.push(op),
        }
    }
    Ok(out)
}

/// 一个重定向 turn:视图生命周期 + 视图侧授权 + turn 边界 diff/merge/discard。
pub struct Turn {
    view: projfs::View,
}

impl Turn {
    pub fn view(&self) -> &projfs::View {
        &self.view
    }

    pub fn root(&self) -> &Path {
        self.view.root()
    }

    /// turn 边界差异(见 [`projfs::View::diff`])。
    pub fn diff(&self) -> io::Result<Vec<projfs::Change>> {
        self.view.diff()
    }

    /// turn 边界合并(见 [`projfs::View::merge`])。
    pub fn merge(&self) -> io::Result<projfs::Report> {
        self.view.merge_with(projfs::MergePolicy::ConfirmDeletions)
    }

    /// turn 边界合并的显式策略形态(ADR-0004:Deleted 项默认跳过,
    /// 调用方确认后以 [`projfs::MergePolicy::AllowDeletions`] 重放)。
    pub fn merge_with(&self, policy: projfs::MergePolicy) -> io::Result<projfs::Report> {
        self.view.merge_with(policy)
    }

    /// turn 边界丢弃(见 [`projfs::View::discard`])。
    pub fn discard(&self) -> io::Result<projfs::Report> {
        self.view.discard()
    }

    /// 取走累计的视图通知(观察流)。
    pub fn take_notifications(&self) -> Vec<projfs::Notification> {
        self.view.take_notifications()
    }

    /// 诊断接口(非契约):对外暴露逐条视图 op 的应用入口,测试用于隔离失败。
    pub fn debug_apply_view_op(&self, v: &ViewOp) -> io::Result<()> {
        apply_view_op(&self.view, v)
    }
}

/// 准备 turn:启动视图 → 物化占位目录 → 视图 DACL 加固 → 逐条应用视图侧 ACE。
///
/// 视图 DACL 加固(protected 重建根 DACL):scratch 挂的 cap-allow ACE
/// (OI CI)会经继承渗进视图根,使未授权路径同样可写(测试实证的逃逸口)。
/// protected 重建后根 DACL 仅含 user ACE(经 (OI)(CI) 继承下发到新对象),
/// 沙箱对"无授权目标"回归无 ACE = 拒绝;父进程凭 user ACE 继续管理视图
/// (provider 服务水合、diff/merge 都由父进程执行)。SYSTEM/Admins 继承
/// ACE 一并清除是 protected 语义的固有代价——scratch 为本进程私有的
/// 临时目录,无其他合法访问者,可接受。
pub fn prepare_turn(store: &Path, view_root: &Path, view_ops: &[ViewOp]) -> io::Result<Turn> {
    let view = projfs::View::start(store, view_root)
        .map_err(|e| io::Error::other(format!("view start: {e}")))?;
    // 先物化(枚举把 store 目录落为占位目录),再挂 ACE——ACE 应用要求
    // 目标对象已存在
    view.materialize_tree()?;
    // 视图根 protected 重建:user 全控(OI CI → 视图内新建/水合对象继承)。
    // 此 ACE 必须先于授权 op:protected 重建会清掉此前的一切(含继承)。
    let user_sid = crate::token::process_user_sid().map_err(io::Error::other)?;
    acl::add_ace_protected(
        view_root,
        &user_sid,
        crate::policy::AceMode::Allow,
        true,
        acl::FULL_MASK,
    )
    .map_err(|e| io::Error::other(format!("view root protected dacl: {e}")))?;
    for v in view_ops {
        apply_view_op(&view, v)
            .map_err(|e| io::Error::other(format!("view op {}: {e}", v.op.path.display())))?;
    }
    Ok(Turn { view })
}

/// 逐组件确保视图内目录存在:已存在(占位或实体)→ 枚举一次触发物化;
/// 不存在 → 建实体目录(upper)。不用 create_dir_all 一步到位:工作区内
/// 已有目录必须走占位物化,避免实体目录遮蔽 store 目录的边界情况。
fn ensure_view_dir_chain(view: &projfs::View, rel_dir: &str) -> io::Result<()> {
    let mut acc = String::new();
    for comp in rel_dir.split('\\').filter(|s| !s.is_empty()) {
        acc = if acc.is_empty() {
            comp.to_string()
        } else {
            format!("{acc}\\{comp}")
        };
        let p = view.root().join(&acc);
        if p.exists() {
            let _ = std::fs::read_dir(&p)?;
        } else {
            std::fs::create_dir(&p)?;
        }
    }
    Ok(())
}

/// 应用一条视图侧 ACE。目标实例化规则:
/// - 目录(Deny/Read/Allow-roots):物化占位或逐组件补建;
/// - 文件(writable_files):store 有同名 → 读一次触发水合(实体化后才能
///   挂 ACE;水合不改内容,diff 按内容比对去噪);store 无 → 预创建空
///   实体(upper),与就地模式的"不存在则预创建"同构。
fn apply_view_op(view: &projfs::View, v: &ViewOp) -> io::Result<()> {
    let op = &v.op;
    let target = view.root().join(&v.rel);
    match op.precreate {
        Precreate::Dir if !target.exists() => ensure_view_dir_chain(view, &v.rel)?,
        Precreate::File => {
            if view.store().join(&v.rel).is_file() {
                // 读一次触发水合(占位 → 实体),之后才能可靠挂 ACE;
                // 内容不变,Modified 判定按内容比对不会误报
                let _ = std::fs::read(&target)?;
            } else if !target.exists() {
                // store 无此文件:预创建(绝不创建实体遮蔽 store 内容)
                if let Some(parent) = v.rel.rsplit_once('\\').map(|(p, _)| p) {
                    ensure_view_dir_chain(view, parent)?;
                }
                std::fs::File::create(&target)?;
            }
        }
        _ => {}
    }
    if !target.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("view ace target missing: {}", op.path.display()),
        ));
    }
    let mask = if op.full_mask {
        acl::FULL_MASK
    } else {
        op.mode.required_mask()
    };
    acl::add_ace_with_mask(&op.path, &op.sid, op.mode, op.inherit_tree, mask)
        .map(|_| ())
        .map_err(|e| io::Error::other(format!("view add_ace {}: {e}", op.path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> SbxPolicy {
        serde_json::from_str(
            r#"{
                "writable_roots": ["C:\\ws\\target", "C:\\out"],
                "writable_files": ["C:\\ws\\a.txt"],
                "deny_write_paths": ["C:\\ws\\.git"],
                "writable_registry_keys": ["HKCU\\Software\\sbx"]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn split_maps_workspace_paths_into_view() {
        let p = policy();
        let split = split_plan(&p, &[1, 2], Path::new("C:\\ws"), Path::new("C:\\view")).unwrap();
        let rels: Vec<&str> = split.view.iter().map(|v| v.rel.as_str()).collect();
        assert_eq!(rels, vec![".git", "target", "a.txt"], "工作区内 → 视图(按策略序)");
        // 视图内路径重写
        assert_eq!(split.view[0].op.path, std::path::PathBuf::from("C:\\view\\.git"));
        assert_eq!(split.view[1].op.path, std::path::PathBuf::from("C:\\view\\target"));
        assert_eq!(split.view[2].op.path, std::path::PathBuf::from("C:\\view\\a.txt"));
        // 视图侧写授权恒 FULL 掩码
        for v in &split.view {
            if v.op.mode == AceMode::Allow {
                assert!(v.op.full_mask);
            }
        }
        // 工作区外 + 注册表 → 就地
        let inplace_paths: Vec<String> = split
            .inplace
            .iter()
            .map(|o| o.path.display().to_string())
            .collect();
        assert_eq!(inplace_paths, vec!["C:\\out", "HKCU\\Software\\sbx"]);
    }

    #[test]
    fn split_is_case_insensitive_and_normalizes_separators() {
        let p = policy();
        let split = split_plan(&p, &[1, 2], Path::new("c:/WS"), Path::new("C:\\view")).unwrap();
        let rels: Vec<&str> = split.view.iter().map(|v| v.rel.as_str()).collect();
        assert_eq!(rels, vec![".git", "target", "a.txt"], "大小写/正斜杠归一");
    }

    #[test]
    fn workspace_rel_shapes() {
        assert_eq!(
            workspace_rel(Path::new("C:\\ws"), Path::new("C:\\ws\\sub\\f.txt")).as_deref(),
            Some("sub\\f.txt")
        );
        assert_eq!(workspace_rel(Path::new("C:\\ws"), Path::new("C:\\ws")).as_deref(), Some(""));
        assert_eq!(workspace_rel(Path::new("C:\\ws"), Path::new("C:\\ws2\\f")), None);
        assert_eq!(workspace_rel(Path::new("C:\\ws"), Path::new("C:\\other\\f")), None);
        assert_eq!(
            workspace_rel(Path::new("C:\\ws\\"), Path::new("c:/ws/x")).as_deref(),
            Some("x")
        );
    }
}
