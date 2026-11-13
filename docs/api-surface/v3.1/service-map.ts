import type { AccountsEmptyParams, ClientsListParams, ConfigDto, ConfigLoadParams, DaemonVersionParams, DiagnosticsMemorySnapshotParams, DsAccountAttemptParams, DsAccountBalanceResult, DsAccountBonus, DsAccountBonusAckParams, DsAccountModel, DsAccountSignInStartParams, DsAccountSignInStartResult, DsAccountSignInStatusResult, DsAccountStatusResult, DsAccountVerifyResult, ExecSessionListParams, ExecSessionListResult, ExecSessionOutputParams, ExecSessionOutputResult, FsEntry, FsListParams, FsReadParams, FsReadResult, MemorySnapshot, PlanReadItem, PlanReadParams, RingingV2ClientView, SessionActivity, SessionActivityParams, SessionDashboard, SessionDashboardParams, SessionGetActivityParams, SessionListEntry, SessionListParams, SessionMetaParams, SessionToolActivity, SessionTurnParams, SessionTurnSnapshot, SiwcFlowParams, SiwcModel, SiwcProfileParams, SiwcProfileSelectParams, SiwcProfilesResult, SiwcSignInStartParams, SiwcSignInStartResult, SiwcSignInStatusResult, SiwcSignOutResult, SiwcStatusResult, SkillsCatalogParams, SkillsCatalogResult, SkillsListToolsParams, TodoListParams, TodoListSnapshot, TodoStatusParams, TodoStatusSnapshot, WorkspaceEntry, WorkspaceGetParams, WorkspaceListParams } from "./index";

export interface ServiceMap {
  "daemon.version": { params: DaemonVersionParams; result: string; };
  "clients.list": { params: ClientsListParams; result: RingingV2ClientView[]; };
  "skills.catalog": { params: SkillsCatalogParams; result: SkillsCatalogResult; };
  "fs.list": { params: FsListParams; result: FsEntry[]; };
  "fs.read": { params: FsReadParams; result: FsReadResult; };
  "session.list": { params: SessionListParams; result: SessionListEntry[]; };
  "session.meta": { params: SessionMetaParams; result: SessionListEntry | null; };
  "session.activity": { params: SessionActivityParams; result: SessionActivity[]; };
  "session.get_activity": { params: SessionGetActivityParams; result: SessionToolActivity[]; };
  "workspace.get": { params: WorkspaceGetParams; result: string; };
  "workspace.list": { params: WorkspaceListParams; result: WorkspaceEntry[]; };
  "config.load": { params: ConfigLoadParams; result: ConfigDto; };
  "session.dashboard": { params: SessionDashboardParams; result: SessionDashboard; };
  "exec_session.list": { params: ExecSessionListParams; result: ExecSessionListResult; };
  "exec_session.output": { params: ExecSessionOutputParams; result: ExecSessionOutputResult; };
  "session.turn": { params: SessionTurnParams; result: SessionTurnSnapshot; };
  "skills.list_tools": { params: SkillsListToolsParams; result: string[]; };
  "todo.list": { params: TodoListParams; result: TodoListSnapshot; };
  "todo.status": { params: TodoStatusParams; result: TodoStatusSnapshot | null; };
  "plan.read": { params: PlanReadParams; result: PlanReadItem[]; };
  "diagnostics.memory.snapshot": { params: DiagnosticsMemorySnapshotParams; result: MemorySnapshot; };
  "siwc.status": { params: AccountsEmptyParams; result: SiwcStatusResult; };
  "siwc.profiles": { params: AccountsEmptyParams; result: SiwcProfilesResult; };
  "siwc.models": { params: SiwcProfileParams; result: SiwcModel[]; };
  "siwc.sign_in.start": { params: SiwcSignInStartParams; result: SiwcSignInStartResult; };
  "siwc.sign_in.status": { params: SiwcFlowParams; result: SiwcSignInStatusResult; };
  "siwc.sign_in.cancel": { params: SiwcFlowParams; result: null; };
  "siwc.profile.select": { params: SiwcProfileSelectParams; result: SiwcStatusResult; };
  "siwc.sign_out": { params: SiwcProfileParams; result: SiwcSignOutResult; };
  "dsaccount.status": { params: AccountsEmptyParams; result: DsAccountStatusResult; };
  "dsaccount.balance": { params: AccountsEmptyParams; result: DsAccountBalanceResult; };
  "dsaccount.bonuses": { params: AccountsEmptyParams; result: DsAccountBonus[]; };
  "dsaccount.sign_in.start": { params: DsAccountSignInStartParams; result: DsAccountSignInStartResult; };
  "dsaccount.sign_in.status": { params: AccountsEmptyParams; result: DsAccountSignInStatusResult; };
  "dsaccount.sign_in.cancel": { params: DsAccountAttemptParams; result: null; };
  "dsaccount.sign_out": { params: AccountsEmptyParams; result: null; };
  "dsaccount.bonus.ack": { params: DsAccountBonusAckParams; result: null; };
  "dsaccount.models": { params: AccountsEmptyParams; result: DsAccountModel[]; };
  "dsaccount.verify": { params: AccountsEmptyParams; result: DsAccountVerifyResult; };
}

export const serviceMetadata = {
  "daemon.version": { kind: "read", requires_session: false, admin_only: false },
  "clients.list": { kind: "read", requires_session: false, admin_only: true },
  "skills.catalog": { kind: "read", requires_session: false, admin_only: false },
  "fs.list": { kind: "read", requires_session: false, admin_only: false },
  "fs.read": { kind: "read", requires_session: false, admin_only: false },
  "session.list": { kind: "read", requires_session: false, admin_only: false },
  "session.meta": { kind: "read", requires_session: true, admin_only: false },
  "session.activity": { kind: "read", requires_session: false, admin_only: false },
  "session.get_activity": { kind: "read", requires_session: true, admin_only: false },
  "workspace.get": { kind: "read", requires_session: true, admin_only: false },
  "workspace.list": { kind: "read", requires_session: false, admin_only: false },
  "config.load": { kind: "read", requires_session: false, admin_only: false },
  "session.dashboard": { kind: "read", requires_session: true, admin_only: false },
  "exec_session.list": { kind: "read", requires_session: true, admin_only: false },
  "exec_session.output": { kind: "read", requires_session: true, admin_only: false },
  "session.turn": { kind: "read", requires_session: true, admin_only: false },
  "skills.list_tools": { kind: "read", requires_session: false, admin_only: false },
  "todo.list": { kind: "read", requires_session: true, admin_only: false },
  "todo.status": { kind: "read", requires_session: true, admin_only: false },
  "plan.read": { kind: "read", requires_session: true, admin_only: false },
  "diagnostics.memory.snapshot": { kind: "read", requires_session: false, admin_only: true },
  "siwc.status": { kind: "read", requires_session: false, admin_only: true },
  "siwc.profiles": { kind: "read", requires_session: false, admin_only: true },
  "siwc.models": { kind: "read", requires_session: false, admin_only: true },
  "siwc.sign_in.start": { kind: "write", requires_session: false, admin_only: true },
  "siwc.sign_in.status": { kind: "read", requires_session: false, admin_only: true },
  "siwc.sign_in.cancel": { kind: "write", requires_session: false, admin_only: true },
  "siwc.profile.select": { kind: "write", requires_session: false, admin_only: true },
  "siwc.sign_out": { kind: "write", requires_session: false, admin_only: true },
  "dsaccount.status": { kind: "read", requires_session: false, admin_only: true },
  "dsaccount.balance": { kind: "read", requires_session: false, admin_only: true },
  "dsaccount.bonuses": { kind: "read", requires_session: false, admin_only: true },
  "dsaccount.sign_in.start": { kind: "write", requires_session: false, admin_only: true },
  "dsaccount.sign_in.status": { kind: "read", requires_session: false, admin_only: true },
  "dsaccount.sign_in.cancel": { kind: "write", requires_session: false, admin_only: true },
  "dsaccount.sign_out": { kind: "write", requires_session: false, admin_only: true },
  "dsaccount.bonus.ack": { kind: "write", requires_session: false, admin_only: true },
  "dsaccount.models": { kind: "read", requires_session: false, admin_only: true },
  "dsaccount.verify": { kind: "write", requires_session: false, admin_only: true },
} as const;
