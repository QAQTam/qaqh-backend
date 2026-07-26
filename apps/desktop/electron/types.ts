export interface DaemonDiscovery {
  endpoint: string;
  token: string;
  pid: number;
  server_epoch: string;
  protocol_version: number;
  daemon_version?: string;
  build_id?: string;
  channel?: string;
  executable?: string;
}

export interface DaemonManifest {
  version: string;
  protocol_version: number;
  build_id: string;
  channel: string;
}

export type ControlMessage = {
  type: string;
  server_epoch?: string;
  seq?: number;
  request_id?: string;
  code?: string;
  message?: string;
  result?: unknown;
  [key: string]: unknown;
};

export interface BackendStatus {
  connected: boolean;
  error?: string;
  updatePending?: boolean;
}

export interface OpenDialogOptions {
  directory?: boolean;
  multiple?: boolean;
  title?: string;
}

export interface ConfirmDialogOptions {
  title?: string;
  kind?: "info" | "warning" | "error";
}
