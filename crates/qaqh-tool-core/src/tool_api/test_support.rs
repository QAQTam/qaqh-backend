//! 测试支撑：descriptor 收集注册表。
//!
//! v4.5 P2-1a 拆除 file-tools / process-tools 对 qaqh-workspace 门面的
//! dev-dependency 反向环后，子 crate 的注册面单测改走本注册表：与
//! `ToolManager::register_typed` 同源（同一 [`TypedToolAdapter::descriptor`]
//! 计算路径），只保留 name → descriptor 与 display 投影，不含权限/会话态。
//! 与具体 `ToolManager` 的耦合验证保留在 `qaqh-workspace/tests` 的集成测试。

use super::RegistersTyped;
use super::descriptor::{ToolDescriptor, ToolExposure};
use super::display::ToolDisplayFn;
use super::erased::ErasedTool;
use super::typed::{TypedTool, TypedToolAdapter};
use std::collections::BTreeMap;

/// 最小注册表：仅执行注册面的 descriptor 投影。
#[derive(Default)]
pub struct DescriptorRegistry {
    tools: BTreeMap<String, ToolDescriptor>,
    display_fns: BTreeMap<String, ToolDisplayFn>,
}

impl DescriptorRegistry {
    /// 查注册 descriptor（字段访问语义对齐 `ToolManager::lookup`）。
    pub fn get(&self, name: &str) -> Option<&ToolDescriptor> {
        self.tools.get(name)
    }

    /// 是否注册（`ToolManager::builtin(..).is_some()` 的同义测试面）。
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// 全部注册工具名（BTreeMap 序，确定性）。
    pub fn names(&self) -> Vec<&str> {
        self.tools.values().map(|d| d.name.as_str()).collect()
    }

    /// 模型面 defs（与 `ToolManager::all_defs` 同律：仅 Direct exposure）。
    pub fn all_defs(&self) -> Vec<qaqh_types::ToolDef> {
        self.tools
            .values()
            .filter(|descriptor| descriptor.exposure == ToolExposure::Direct)
            .map(|descriptor| qaqh_types::ToolDef {
                call_type: "function".into(),
                function: qaqh_types::ToolFunction {
                    name: descriptor.name.as_str().to_owned(),
                    description: descriptor.description.clone(),
                    parameters: descriptor.input_schema.clone(),
                },
            })
            .collect()
    }

    /// 注册到名下的 display 投影器（展示契约断言用）。
    pub fn display_fn(&self, name: &str) -> Option<ToolDisplayFn> {
        self.display_fns.get(name).copied()
    }
}

impl RegistersTyped for DescriptorRegistry {
    fn register_typed_tool<T: TypedTool + 'static>(&mut self, tool: T) {
        let descriptor = TypedToolAdapter::new(tool).descriptor();
        self.tools.insert(descriptor.name.as_str().to_owned(), descriptor);
    }

    fn register_display_fn(&mut self, name: &str, projector: ToolDisplayFn) {
        self.display_fns.insert(name.to_owned(), projector);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_api::{
        ToolCallContext, ToolContentBlock, ToolExecutionError, ToolMeta, ToolProjection,
    };
    use crate::permission::ToolCategory;
    use crate::ToolRisk;

    #[derive(serde::Serialize, schemars::JsonSchema)]
    struct PingOutput {
        text: String,
    }

    impl ToolProjection for PingOutput {
        fn model_blocks(&self) -> Vec<ToolContentBlock> {
            vec![ToolContentBlock::Text {
                text: self.text.clone(),
            }]
        }
    }

    struct PingTool;

    impl TypedTool for PingTool {
        type Args = serde_json::Value;
        type Output = PingOutput;

        fn meta(&self) -> ToolMeta {
            ToolMeta::new(
                "ping_test_support",
                "ping",
                ToolCategory::Read,
                ToolRisk::ReadOnly,
                std::time::Duration::from_secs(1),
            )
        }

        fn run(
            &self,
            _ctx: &ToolCallContext,
            args: Self::Args,
        ) -> Result<Self::Output, ToolExecutionError> {
            Ok(PingOutput {
                text: args.to_string(),
            })
        }
    }

    #[test]
    fn registry_collects_descriptors_from_register_typed() {
        let mut registry = DescriptorRegistry::default();
        registry.register_typed_tool(PingTool);
        assert!(registry.contains("ping_test_support"));
        let descriptor = registry.get("ping_test_support").expect("registered");
        assert_eq!(descriptor.description, "ping");
        assert_eq!(registry.all_defs().len(), 1);
        assert_eq!(registry.names(), vec!["ping_test_support"]);
    }
}
