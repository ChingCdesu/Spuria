// Typed wrappers over the signaling admin HTTP API. The dashboard is served
// from the same origin as the API, so paths are relative.

export interface Stats {
  online: number;
  sessions: number;
}
export interface Device {
  device_id: string;
  noise_pubkey: string;
  uptime_secs: number;
  idle_secs: number;
}
export interface Session {
  session_id: string;
  controller: string;
  host: string;
  path: "p2p" | "relay" | null;
  age_secs: number;
}
export interface Audit {
  ts_ms: number;
  kind: string;
  detail: string;
}

export class Unauthorized extends Error {}

function headers(token: string): HeadersInit {
  return { Authorization: "Bearer " + token };
}

export async function apiGet<T>(path: string, token: string): Promise<T> {
  const r = await fetch(path, { headers: headers(token) });
  if (r.status === 401) throw new Unauthorized("unauthorized");
  if (!r.ok) throw new Error("HTTP " + r.status);
  return (await r.json()) as T;
}

export async function kick(deviceId: string, token: string): Promise<void> {
  const r = await fetch("/api/kick", {
    method: "POST",
    headers: { ...headers(token), "Content-Type": "application/json" },
    body: JSON.stringify({ device_id: deviceId }),
  });
  if (r.status === 401) throw new Unauthorized("unauthorized");
  if (!r.ok) throw new Error("HTTP " + r.status);
}

export function fmtDur(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ${secs % 60}s`;
  return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
}
