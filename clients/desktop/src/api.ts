// Typed wrappers over the Tauri commands and event stream exposed by
// `src-tauri/src/main.rs`.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface Settings {
  // Network
  server: string;
  reflect: string;
  secret: string;
  // Connection defaults
  default_listen: string;
  default_rdp: string;
  force_relay: boolean;
  enable_udp: boolean;
  // Appearance: "system" | "light" | "dark"
  theme: string;
  // Updates
  auto_check_updates: boolean;
}

export interface AppInfo {
  version: string;
  device_id: string;
  rdp_launch_supported: boolean;
}

export type Role = "controller" | "host";

export interface ConnectOpts {
  role: Role;
  device_id?: string | null;
  peer_id?: string | null;
  listen?: string | null;
  rdp?: string | null;
  force_relay: boolean;
  rdp_launch?: { username: string; password: string } | null;
  allow_forward_ports?: number[];
}

export interface ForwardInfo {
  forward_id: string;
  listen_addr: string;
  remote_port: number;
}

export interface RdpLaunchEvent {
  session_id: string;
  status: "launched" | "failed";
  message?: string;
}

export type ClientEvent =
  | { kind: "registered"; device_id: string }
  | { kind: "session_started"; session_id: string; peer_id: string }
  | { kind: "tunnel_up"; session_id: string; path: string }
  | { kind: "rdp_ready"; session_id: string; listen_addr: string }
  | { kind: "host_bridging"; session_id: string; rdp_addr: string }
  | { kind: "port_forwarding_available"; session_id: string; available: boolean }
  | ({ kind: "port_forward_started"; session_id: string } & ForwardInfo)
  | { kind: "port_forward_stopped"; session_id: string; forward_id: string }
  | { kind: "port_forward_error"; session_id: string; forward_id: string; message: string }
  | { kind: "session_ended"; session_id: string; error?: string | null }
  | { kind: "error"; message: string };

export const api = {
  getSettings: () => invoke<Settings>("get_settings"),
  saveSettings: (settings: Settings) => invoke<void>("save_settings", { settings }),
  getAppInfo: () => invoke<AppInfo>("get_app_info"),
  ensureDeviceId: () => invoke<string>("ensure_device_id"),
  connect: (opts: ConnectOpts) => invoke<string>("connect", { opts }),
  disconnect: () => invoke<void>("disconnect"),
  startPortForward: (sessionId: string, listenAddr: string, remotePort: number) =>
    invoke<ForwardInfo>("start_port_forward", { sessionId, listenAddr, remotePort }),
  stopPortForward: (sessionId: string, forwardId: string) =>
    invoke<void>("stop_port_forward", { sessionId, forwardId }),
  checkUpdate: () => invoke<string>("check_update"),
};

export function onClientEvent(cb: (e: ClientEvent) => void): Promise<UnlistenFn> {
  return listen<ClientEvent>("client-event", (e) => cb(e.payload));
}
export function onClientStopped(cb: () => void): Promise<UnlistenFn> {
  return listen("client-stopped", () => cb());
}
export function onClientError(cb: (msg: string) => void): Promise<UnlistenFn> {
  return listen<string>("client-error", (e) => cb(e.payload));
}
export function onRdpLaunch(cb: (event: RdpLaunchEvent) => void): Promise<UnlistenFn> {
  return listen<RdpLaunchEvent>("rdp-launch", (event) => cb(event.payload));
}
