/**
 * Presentation-only command catalogue for the composer.
 *
 * Command execution is deliberately not coupled here: future contextual
 * registrations can extend this catalogue and provide an executor separately.
 */
export interface SlashCommand {
  command: string;
  label: string;
  description: string;
}

export const SLASH_COMMANDS: readonly SlashCommand[] = [
  { command: "/settings", label: "设置", description: "打开应用设置" },
  { command: "/model", label: "模型", description: "切换对话模型" },
  { command: "/effort", label: "强度", description: "调整推理强度" },
  { command: "/usage", label: "用量", description: "查看用量图表" },
];

/** Returns menu candidates only when the composer begins with a slash. */
export function matchSlashCommands(value: string): readonly SlashCommand[] {
  if (!value.startsWith("/")) return [];
  const query = value.slice(1).trim().toLocaleLowerCase();
  return SLASH_COMMANDS.filter(item =>
    !query || item.command.slice(1).toLocaleLowerCase().includes(query) || item.label.includes(query)
  );
}
