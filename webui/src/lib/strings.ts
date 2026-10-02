/**
 * 固定词表（spec §16）。新增文案 MUST 先加入这里；
 * 同一动作全程同名（用户点「停止」，状态显示「已中断」）。
 */
export const STR = {
  newTab: "新对话",
  running: "运行中",
  waitingYou: "等待你的处理",
  error: "出错",
  hasNewReply: "有新回复",
  reconnecting: "重连中",
  disconnected: "已断开",
  retry: "重试",
  worked: (parts: string) => `已工作${parts ? ` ${parts}` : ""}`,
  workedAbbrev: "已工作",
  interrupted: "已中断",
  interruptedWorked: (parts: string) => `已中断,工作 ${parts}`,
  failedWorked: (parts: string) => `出错,工作 ${parts}`,
  failed: "失败",
  denied: "已拒绝",
  aborted: "已中断",
  backgrounded: "已转后台",
  prepare: "准备中",
  done: "完成",
  thinking: "思考",
  toolCallsAndThinking: (tools: number, thinkings: number) => {
    const seg: string[] = [];
    if (tools > 0) seg.push(`${tools} 次工具调用`);
    if (thinkings > 0) seg.push(`${thinkings} 段思考`);
    return seg.join(",");
  },
  waitApproval: (d: string) => `等待授权 ${d}`,
  waitAnswer: (d: string) => `等待回答 ${d}`,
  morePending: (n: number) => `还有 ${n} 个待处理`,
  allowOnce: "允许一次",
  allowSession: "本会话允许",
  allowTrust: "批准并信任该文件夹",
  reject: "拒绝",
  submit: "提交",
  skip: "跳过",
  stop: "停止",
  send: "发送",
  copy: "复制",
  copied: "已复制",
  expand: "展开",
  collapse: "收起",
  backToBottom: "回到底部",
  loadFailedRetry: "加载失败,点击重试",
  loadOlderFailed: "加载失败,点击重试",
  truncatedLines: (total: number, kept: number) => `已截断:共 ${total} 行,显示 ${kept} 行`,
  exit: (code: number) => `exit ${code}`,
  stdout: "输出",
  stderr: "错误输出",
  input: "输入",
  output: "输出",
  params: "参数",
  changes: "变更",
  highRisk: "高风险",
  riskLabel: "风险",
  reason: "原因",
  consequence: "后果",
  paths: "路径",
  approvalTitle: "执行授权",
  askTitle: "回答问题",
  planTitle: "计划评审",
  planPlaceholder: "审批意见(可选)",
  autonomous: "自主执行",
  customAnswer: "自定义回答",
  noSession: "没有可用会话,点击重试。",
  bootFailed: "连接失败,点击重试。",
  sendDisabledRunning: "回合运行中,等待结束后发送",
  sendDisabledOffline: "连接断开,无法发送",
  sendDisabledApproval: "有待处理的请求,先处理后发送",
  compactedAbove: "此前已压缩",
  truncatedWindow: "更早的回合未在此窗口内",
  decidedAllow: "已允许",
  decidedDeny: "已拒绝",
  statusUnknown: "状态未知",
  binaryChanged: "二进制文件已变更",
  incompatibleDaemon: (detail: string) => `检测到不兼容的 daemon 实例(${detail})。桌面壳需要匹配的版本。`,
  incompatibleStopAndConnect: "停止旧实例并连接",
  incompatibleBusy: "旧实例正在运行任务,无法自动停止。请退出正在使用的会话后重试,或手动退出旧实例。",
  incompatibleStopping: "正在停止旧实例…",
  hostError: "桌面壳与 daemon 通信异常",
} as const;

/** 「已工作 X 秒」的时长格式化（spec §7.3）。纯函数,可单测。 */
export function formatWorkDuration(ms: number): string {
  if (ms < 1_000) return "不到 1 秒";
  const totalSeconds = Math.floor(ms / 1_000);
  if (totalSeconds < 60) return `${totalSeconds} 秒`;
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  if (minutes < 60) return seconds > 0 ? `${minutes} 分 ${seconds} 秒` : `${minutes} 分`;
  const hours = Math.floor(minutes / 60);
  const restMinutes = minutes % 60;
  return restMinutes > 0 ? `${hours} 小时 ${restMinutes} 分` : `${hours} 小时`;
}

/** 时间线右侧相对偏移:+3.2s（spec §7.3.1）。 */
export function formatOffset(ms: number): string {
  if (ms < 0) ms = 0;
  if (ms < 100_000) return `+${(ms / 1_000).toFixed(1)}s`;
  const minutes = Math.floor(ms / 60_000);
  const seconds = Math.floor((ms % 60_000) / 1_000);
  return `+${minutes}m${seconds.toString().padStart(2, "0")}s`;
}
