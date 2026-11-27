//! exec 输出**权威日志**的路径规则单源（[ADR-0017] 决策 1 / [ADR-0034] 决策 3）。
//!
//! 三个消费者必须说同一套路径，否则「权威日志」就成了三份各自为政的猜想：
//! - 写者：owner 泵的 `append_exec_log`（全量 append-only 落盘）；
//! - 读者：runtime 的 `exec_session.output` 区间读（前端补读主路径）；
//! - 告示牌：ADR-0035 泄洪通告——它把模型指到日志上，路径错了闸门就是在误导模型。
//!
//! 定位规则：`<data_dir>/sessions/<session_id>/exec/<exec_session_id>.log`。
//! `data_dir` 由业务库路径派生（`qaqh.db` 的父目录）；两类 id 都是内核生成的
//! 单段路径组件，因此直接拼接不引入穿越风险。
//!
//! [ADR-0017]: ../../docs/adr/0017-exec-full-output-terminal-lifecycle.md
//! [ADR-0034]: ../../docs/adr/0034-exec-flood-memory-surface.md

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 单会话 exec 日志目录：`<data_dir>/sessions/<session_id>/exec`。
pub fn exec_output_log_dir(data_dir: &Path, session_id: &str) -> PathBuf {
    data_dir.join("sessions").join(session_id).join("exec")
}

/// 单 exec 会话日志文件：`<data_dir>/sessions/<session_id>/exec/<exec_session_id>.log`。
pub fn exec_output_log_path_in(
    data_dir: &Path,
    session_id: &str,
    exec_session_id: &str,
) -> PathBuf {
    exec_output_log_dir(data_dir, session_id).join(format!("{exec_session_id}.log"))
}

/// 追加一块输出到日志。定序不变量（ADR-0017 决策 1）：先写日志、再更新 DB 水位、
/// 再发 delta ⇒「客户端见过 offset X 的 delta ⇒ 日志必可以读到 X」。
/// 失败只响亮报错、不阻断实时流；完整性由 settle 侧「日志字节 vs 水位」对账
/// 如实标 `output_truncated`（宪规 5：不假装完整）。
pub(crate) fn append_exec_log(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_path_is_derived_from_one_rule() {
        let data_dir = Path::new("/data");
        assert_eq!(
            exec_output_log_path_in(data_dir, "SES-1", "execs_01A"),
            PathBuf::from("/data/sessions/SES-1/exec/execs_01A.log")
        );
        assert_eq!(
            exec_output_log_dir(data_dir, "SES-1"),
            PathBuf::from("/data/sessions/SES-1/exec")
        );
    }
}
