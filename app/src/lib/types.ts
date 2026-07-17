// TypeScript mirrors of the Rust types crossing the Tauri bridge.
// Field names must match the serde output (rename_all = "camelCase" on
// structs; Event uses `type` for event_type). If you change a Rust type,
// change it here.

export interface AnimSpec {
  effect: string; // off|solid|breathe|rainbow|chase|sparkle|flash|gradient|progress|dual_progress
  color: [number, number, number];
  color2: [number, number, number] | null;
  speed: number; // 0..1
  progress: number | null; // "progress" fill; "dual_progress" left bar
  progress2: number | null; // "dual_progress" right bar
  keyframes: [number, number, number][] | null; // "keyframes" color stops
}

export interface DeviceStatus {
  connected: boolean;
  port: string | null;
  serialNumber: string | null;
  fwVersion: string | null;
  ledCount: number | null;
  protocolVersion: number | null;
}

export interface PortCandidate {
  port: string;
  serialNumber: string | null;
  product: string | null;
}

/** A named visual: effect + colors + speed. */
export interface Animation {
  id: number;
  name: string;
  spec: AnimSpec;
  builtin: boolean;
  /** Default length wherever it's shown (null = until outranked/released). */
  durationMs: number | null;
}

/** A clock-driven action: emit a time/* event, or swap the idle animation. */
export interface Schedule {
  id: number;
  name: string;
  time: string; // "HH:MM" local, daily
  action: "emit" | "idle";
  eventType: string | null;
  animationId: number | null;
  enabled: boolean;
}

/** Event → animation mapping with priority and optional expiry. */
export interface Trigger {
  id: number;
  name: string;
  source: string;
  eventType: string;
  clearEventType: string | null;
  animationId: number;
  priority: number;
  durationMs: number | null;
  enabled: boolean;
  policy: {
    profile: string;
    payloadPath: string | null;
    payloadEquals: unknown | null;
    cooldownMs: number | null;
  };
}

export interface BusEvent {
  source: string;
  type: string;
  payload: unknown;
  ts: number;
}

export interface IntegrationHealth {
  source: string;
  status: "healthy" | "error" | "not_configured";
  message: string | null;
  lastAttemptMs: number | null;
  lastSuccessMs: number | null;
}
export interface IntegrationDescriptor { source: string; name: string; setup: string; events: string[]; }
export interface KnownDevice { serialNumber: string | null; lastPort: string; ledCount: number; fwVersion: string; lastSeenMs: number; }

export interface OverlayInfo {
  key: string;
  name: string;
  priority: number;
  expiresInMs: number | null;
  winning: boolean;
}

export interface ActiveState {
  activeName: string;
  snoozedUntilMs: number | null;
  overlays: OverlayInfo[];
  quietHoursActive: boolean;
}

export function rgbToHex([r, g, b]: [number, number, number]): string {
  const h = (n: number) => n.toString(16).padStart(2, "0");
  return `#${h(r)}${h(g)}${h(b)}`;
}

export function hexToRgb(hex: string): [number, number, number] {
  const v = hex.replace("#", "");
  return [
    parseInt(v.slice(0, 2), 16),
    parseInt(v.slice(2, 4), 16),
    parseInt(v.slice(4, 6), 16),
  ];
}
