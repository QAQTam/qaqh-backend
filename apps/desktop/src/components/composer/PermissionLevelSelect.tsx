export const PERMISSION_LEVELS = [
  { value: 1, label: "L1 全部询问" },
  { value: 2, label: "L2 读取免询问" },
  { value: 3, label: "L3 工作区操作" },
  { value: 4, label: "L4 完全访问" },
] as const;

export default function PermissionLevelSelect(props: {
  level: number;
  onChange: (level: number) => void | Promise<void>;
  compact?: boolean;
}) {
  return (
    <label
      class={{ "permission-level-select": true, compact: props.compact ?? false, "is-danger": props.level === 4 }}
      data-permission-level={props.level}
      title="控制 DeepX 可执行的操作范围"
    >
      <span class="permission-level-label">权限</span>
      <select
        aria-label="权限等级"
        value={props.level}
        onChange={(event) => void props.onChange(Number(event.currentTarget.value))}
      >
        {PERMISSION_LEVELS.map((item) => (
          <option value={item.value}>{item.label}</option>
        ))}
      </select>
    </label>
  );
}
