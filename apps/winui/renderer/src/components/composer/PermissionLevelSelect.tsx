export const PERMISSION_LEVELS = [
  { value: 1, label: "L1", desc: "全部询问", color: "l1" },
  { value: 2, label: "L2", desc: "读取免问", color: "l2" },
  { value: 3, label: "L3", desc: "工作区", color: "l3" },
  { value: 4, label: "L4", desc: "完全访问", color: "l4" },
] as const;

export default function PermissionLevelSelect(props: {
  level: number;
  onChange: (level: number) => void | Promise<void>;
  compact?: boolean;
}) {
  return (
    <div
      class={[
        "permission-level-select",
        { compact: props.compact === true, "is-danger": props.level === 4 },
      ]}
      data-permission-level={props.level}
      title="控制 DeepX 可执行的操作范围"
      role="radiogroup"
      aria-label="权限等级"
    >
      <span class="permission-level-label">权限</span>
      <div class="permission-pills">
        {PERMISSION_LEVELS.map((item) => (
          <button
            type="button"
            class={[`permission-pill pill-${item.color}`, { active: props.level === item.value }]}
            data-permission-option={item.value}
            role="radio"
            aria-checked={props.level === item.value ? "true" : "false"}
            onClick={() => void props.onChange(item.value)}
            title={item.desc}
          >
            <span class="pill-label">{item.label}</span>
            {!props.compact && <span class="pill-desc">{item.desc}</span>}
          </button>
        ))}
      </div>
    </div>
  );
}
