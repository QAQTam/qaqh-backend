export interface SubtitleCue {
  id: string;
  start: number;
  end: number;
  text: string;
  highlight?: string;
  act: number;
}

export interface VersionKeyframe {
  x: number; // Normalized timeline position [0, 1]
  y: number; // Visual height representing version milestone
  date: string;
  version: string;
  brand: 'DSX' | 'DeepX' | 'QAQ-Harness';
  label: string;
  kind: 'peak' | 'crash' | 'milestone' | 'normal';
}

export interface HudState {
  start: number;
  end: number;
  act: string;
  brand: 'DSX' | 'DeepX' | 'QAQ-Harness';
  dayText: string;
  dateText: string;
  versionText: string;
  versionTone: 'cyan' | 'gold' | 'crimson' | 'emerald';
}

export const TOTAL_DURATION = 180.0;

export const REPO_METRICS = {
  days: 184,
  activeDays: 92,
  mainlineCommits: 1426,
  totalRecoveredCommits: 1426,
  crates: 40,
  rustFiles: 466,
  rustLoc: 154159,
  renames: 3,
  fourZeroClimbs: 3,
  contractsV4: 612,
  adrs: 20,
  graftedA13Commits: 298,
  strippedV4Lines: 30597,
  buriedLoopLines: 9100,
  daemonRssMb: 25.8,
  electronHeapMb: 171,
  claudeCodeSpawnMb: 1024,
  deepseekOverdraftCny: '-¥0.69',
  deepseekMaySpendCny: '¥358.59',
};

export const HUD_STATES: HudState[] = [
  {
    start: 0,
    end: 15,
    act: 'ACT 01 // HOOK 15S',
    brand: 'QAQ-Harness',
    dayText: 'DAY 001 → 184',
    dateText: '184 DAYS · GEMINI 4',
    versionText: 'v4.0.0 ×3',
    versionTone: 'gold',
  },
  {
    start: 15,
    end: 34,
    act: 'ACT 02 // WHAT IT IS',
    brand: 'QAQ-Harness',
    dayText: 'MULTI-CLIENT',
    dateText: '2026.10.08 03:07',
    versionText: '25.8 MB RSS',
    versionTone: 'cyan',
  },
  {
    start: 34,
    end: 54,
    act: 'ACT 03 // ORIGIN DSX',
    brand: 'DSX',
    dayText: 'DAY 001',
    dateText: '2026.05.14 → 05.18',
    versionText: 'v4.0.0-alpha1',
    versionTone: 'emerald',
  },
  {
    start: 54,
    end: 72,
    act: 'ACT 04 // BUN·GO·FOMO',
    brand: 'DeepX',
    dayText: 'MAY → JUN 02',
    dateText: 'REASONIX CRISIS',
    versionText: 'DeepX v4.0',
    versionTone: 'gold',
  },
  {
    start: 72,
    end: 92,
    act: 'ACT 05 // DISASTER',
    brand: 'QAQ-Harness',
    dayText: 'JUN → AUG 22',
    dateText: '2026.08.22 02:30',
    versionText: '0.0.0-alpha0',
    versionTone: 'crimson',
  },
  {
    start: 92,
    end: 108,
    act: 'ACT 06 // RINGING V2',
    brand: 'QAQ-Harness',
    dayText: 'SEP 23 → NOV 03',
    dateText: 'CONTRACT REBIRTH',
    versionText: 'v3.0 LTS',
    versionTone: 'cyan',
  },
  {
    start: 108,
    end: 123,
    act: 'ACT 07 // OVERCODER UI',
    brand: 'QAQ-Harness',
    dayText: '15S MOCK',
    dateText: 'WINUI · MICA · HANDOFF',
    versionText: 'Overcoder',
    versionTone: 'cyan',
  },
  {
    start: 123,
    end: 148,
    act: 'ACT 08 // ARCHAEOLOGY',
    brand: 'QAQ-Harness',
    dayText: 'NOV 12 → 13',
    dateText: 'ADR-0013 + ADR-0020',
    versionText: '1,426 COMMITS',
    versionTone: 'gold',
  },
  {
    start: 148,
    end: 180,
    act: 'ACT 09 // 3RD 4.0',
    brand: 'QAQ-Harness',
    dayText: 'DAY 184',
    dateText: '2026.11.14 15:00',
    versionText: 'v4.0.0',
    versionTone: 'gold',
  },
];

export const VERSION_CURVE: VersionKeyframe[] = [
  { x: 0.02, y: 2.2, date: '05-10', version: 'DSC v2.2', brand: 'DSX', label: 'Arch TTY 初代', kind: 'normal' },
  { x: 0.11, y: 4.0, date: '05-18', version: 'v4.0.0', brand: 'DSX', label: '第 1 次 4.0', kind: 'peak' },
  { x: 0.23, y: 4.0, date: '06-02', version: 'v4.0', brand: 'DeepX', label: '第 2 次 4.0', kind: 'peak' },
  { x: 0.33, y: 2.4, date: '06-28', version: '74KB 巨石', brand: 'DeepX', label: '黑暗 71 天', kind: 'crash' },
  { x: 0.44, y: 0.9, date: '07-26', version: '0.9.0', brand: 'DeepX', label: '崩溃坠崖', kind: 'crash' },
  { x: 0.52, y: 1.0, date: '08-16', version: '1.0-rc.8', brand: 'QAQ-Harness', label: '改名 QAQH', kind: 'milestone' },
  { x: 0.59, y: 0.0, date: '08-22', version: '0.0.0-α0', brand: 'QAQ-Harness', label: '凌晨删库归零', kind: 'crash' },
  { x: 0.69, y: 2.0, date: '09-23', version: '2.0.0-α1', brand: 'QAQ-Harness', label: 'Ringing V2', kind: 'milestone' },
  { x: 0.77, y: 2.7, date: '10-31', version: '-9.1k 行', brand: 'QAQ-Harness', label: '埋葬旧 Loop', kind: 'milestone' },
  { x: 0.85, y: 3.1, date: '11-03', version: '3.0 LTS', brand: 'QAQ-Harness', label: '契约锁门', kind: 'milestone' },
  { x: 0.92, y: 3.3, date: '11-13', version: '3.1+考古', brand: 'QAQ-Harness', label: '贯通 1426 提交', kind: 'milestone' },
  { x: 0.99, y: 4.0, date: '11-14', version: '4.0.0', brand: 'QAQ-Harness', label: '第 3 次 4.0', kind: 'peak' },
];

export const SUBTITLE_CUES: SubtitleCue[] = [
  // ACT 1: 0 - 15s (Douyin 15s Hook + Gemini 4 Hydrogen)
  { id: 'c01', start: 0.2, end: 3.4, act: 1, text: '嫌 Claude Code 一开吃 1G 内存，', highlight: '1G 内存' },
  { id: 'c02', start: 3.4, end: 6.9, act: 1, text: '随手写的小代理竟长成 15 万行 Rust！', highlight: '15 万行 Rust' },
  { id: 'c03', start: 6.9, end: 11.1, act: 1, text: '恰逢 Gemini 4 Hydrogen 发布，全片由它编排。', highlight: 'Gemini 4 Hydrogen' },
  { id: 'c04', start: 11.1, end: 14.8, act: 1, text: '三度改名三上 4.0，184 天胡闹编程开演！', highlight: '三上 4.0' },

  // ACT 2: 15 - 34s (What is QAQ-Harness?)
  { id: 'c05', start: 15.2, end: 18.2, act: 2, text: 'QAQ-Harness 到底是什么？', highlight: 'QAQ-Harness' },
  { id: 'c06', start: 18.2, end: 23.4, act: 2, text: '常驻 26 兆的 Rust 内核，用 Ringing V2 驱动全端。', highlight: '26 兆' },
  { id: 'c07', start: 23.4, end: 27.6, act: 2, text: '看 10 月 8 号凌晨实机截图，同一会话，', highlight: '10 月 8 号' },
  { id: 'c08', start: 27.6, end: 33.4, act: 2, text: '电脑、安卓、鸿蒙同屏接力，这就是开发驾驶舱！', highlight: '同屏接力' },

  // ACT 3: 34 - 54s (Origin Story: ArchTTY, -0.69 CNY, May 14 Split -> DSX 4.0)
  { id: 'c09', start: 34.2, end: 38.6, act: 3, text: '起初只是 Arch 终端小脚本，黑屏白字修启动。', highlight: 'Arch 终端' },
  { id: 'c10', start: 38.6, end: 43.4, act: 3, text: '五月烧光三百五十八、欠费六毛九，越调越上头。', highlight: '欠费六毛九' },
  { id: 'c11', start: 43.4, end: 48.8, act: 3, text: '五月十四号连派三十个子代理，将八十源码拆成五进程，', highlight: '三十个子代理' },
  { id: 'c12', start: 48.8, end: 53.5, act: 3, text: '高喊超越 Claude Code，当天 DSX 首上 4.0！', highlight: 'DSX 首上 4.0' },

  // ACT 4: 54 - 72s (Bun / Go Doubt & Late-May Reasonix FOMO)
  { id: 'c13', start: 54.2, end: 59.5, act: 4, text: '很快 Rust 慢编译磨光耐心：要不要改写 Bun 或 Go？', highlight: 'Bun 或 Go' },
  { id: 'c14', start: 59.5, end: 65.5, act: 4, text: '五月底 Reasonix 爆火，看着 Go 一骑绝尘差点彻底弃坑。', highlight: 'Reasonix 爆火' },
  { id: 'c15', start: 65.5, end: 71.4, act: 4, text: '改名 DeepX 二次上 4.0，紧接着跌进六月黑洞！', highlight: '二次上 4.0' },

  // ACT 5: 72 - 92s (June-July Disaster & 08-22 Nuke)
  { id: 'c16', start: 72.2, end: 76.8, act: 5, text: '六七月彻底失控：旧仓丢失，仅剩单体文件苦撑。', highlight: '单体文件' },
  { id: 'c17', start: 76.8, end: 81.6, act: 5, text: '硬扛三套兼容，七月底版本雪崩坠崖到零点九！', highlight: '雪崩坠崖到零点九' },
  { id: 'c18', start: 81.6, end: 86.5, act: 5, text: '八月改写原生桌面，三度更名 QAQ-Harness。', highlight: 'QAQ-Harness' },
  { id: 'c19', start: 86.5, end: 91.5, act: 5, text: '二十二号凌晨，一念之下删光仓库，彻底归零！', highlight: '彻底归零' },

  // ACT 6: 92 - 108s (Ringing V2 Rebirth & Technical Reconciliation)
  { id: 'c20', start: 92.2, end: 97.2, act: 6, text: '九月破而后立，定下 Ringing V2 与 SQLite 单源权威。', highlight: 'Ringing V2' },
  { id: 'c21', start: 97.2, end: 102.2, act: 6, text: '十月底砍掉九千行旧循环，立起七道机械闸门！', highlight: '七道机械闸门' },
  { id: 'c22', start: 102.2, end: 107.4, act: 6, text: '曾眼红的 Reasonix 与 Bun 沙箱，也一起融进内核！', highlight: '融进内核' },

  // ACT 7: 108 - 123s (15s Overcoder Frontend Mock Showcase: WinUI + Mica + Handoff)
  { id: 'c23', start: 108.2, end: 112.5, act: 7, text: '看这十五秒：新前端 Overcoder，WinUI 动效与全局 Mica！', highlight: '全局 Mica' },
  { id: 'c24', start: 112.5, end: 117.2, act: 7, text: '上下文触顶触发封箱，摘要折进木箱咔哒落锁，', highlight: '触发封箱' },
  { id: 'c25', start: 117.2, end: 122.4, act: 7, text: '随即飞入侧栏新分支，开箱继承，接着上次的活！', highlight: '接着上次的活' },

  // ACT 8: 123 - 148s (Cyber Archaeology: ADR-0013 + ADR-0020)
  { id: 'c26', start: 123.2, end: 128.8, act: 8, text: '半周年前夕，我们展开赛博考古，把删掉的记忆接回现实。', highlight: '赛博考古' },
  { id: 'c27', start: 128.8, end: 135.2, act: 8, text: '用 filter-repo 接回 298 次提交，再用空气提交桥接六月黑盒。', highlight: '298 次提交' },
  { id: 'c28', start: 135.2, end: 141.2, act: 8, text: '十五兆历史对话全量脱敏，一千四百余次主干焊成完整时间线。', highlight: '完整时间线' },
  { id: 'c29', start: 141.2, end: 147.4, act: 8, text: '核验结果：协议语义留存八成，八大核心子系统百分百延续！', highlight: '百分百延续' },

  // ACT 9: 148 - 180s (3rd Climb to v4.0.0 & Half-Year Finale)
  { id: 'c30', start: 148.2, end: 154.0, act: 9, text: '来到今天半周年：拆掉三点几冻结闸门与三万行包袱，', highlight: '三万行包袱' },
  { id: 'c31', start: 154.0, end: 160.2, act: 9, text: '机器重铸 612 份契约制品，时隔 184 天第三次站上 4.0！', highlight: '第三次站上 4.0' },
  { id: 'c32', start: 160.2, end: 166.8, act: 9, text: '从终端小脚本到四十个 Crate、十五万行代码、一千四百次心跳，', highlight: '一千四百次心跳' },
  { id: 'c33', start: 166.8, end: 173.2, act: 9, text: '代码会删、版本会归零，只要让每个前端共用同一个真诚内核，', highlight: '共用同一个真诚内核' },
  { id: 'c34', start: 173.2, end: 179.5, act: 9, text: '这半年胡闹编程就比任何保守都算数！Gemini 4 制作，4.0 见！', highlight: '4.0 见' },
];
