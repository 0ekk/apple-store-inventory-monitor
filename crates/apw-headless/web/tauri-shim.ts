/**
 * Browser stand-in for the three Tauri modules the desktop UI imports
 * (`@tauri-apps/api/core`, `@tauri-apps/api/event`,
 * `@tauri-apps/plugin-opener`), so upstream's `src/` builds for the headless
 * web UI without a single edit. `vite.config.ts` next to this file aliases
 * all three module names here.
 *
 *   invoke(name, args) -> POST /api/v1/commands/{name}, args as the JSON body
 *   listen(channel)    -> one shared EventSource on /api/v1/events, where each
 *                         SSE event name is a Tauri channel name
 *   openUrl(url)       -> a new browser tab
 *
 * Server side: crates/apw-headless/src/web.rs. Auth is HTTP Basic; the
 * browser re-sends the credentials on fetch and EventSource by itself.
 */

export type UnlistenFn = () => void;

export interface Event<T> {
  event: string;
  id: number;
  payload: T;
}

/** Commands whose desktop version opens a page on the app's own machine. */
const OPENS_RETURNED_URL = new Set(["open_target_product"]);

export async function invoke<T>(cmd: string, args: Record<string, unknown> = {}): Promise<T> {
  const response = await fetch(`/api/v1/commands/${encodeURIComponent(cmd)}`, {
    method: "POST",
    // Required by the server, and the reason cross-site pages can't call it:
    // this content type forces a CORS preflight the server never approves.
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(args),
  });
  const body: unknown = await response.json().catch(() => `HTTP ${response.status}`);
  // Tauri rejects with the command's error string; keep that contract.
  if (!response.ok) throw body;
  if (OPENS_RETURNED_URL.has(cmd) && typeof body === "string") await openUrl(body);
  return body as T;
}

let source: EventSource | null = null;

export async function listen<T>(
  channel: string,
  handler: (event: Event<T>) => void,
): Promise<UnlistenFn> {
  source ??= new EventSource("/api/v1/events");
  const stream = source;
  const listener = (message: MessageEvent<string>) =>
    handler({ event: channel, id: 0, payload: JSON.parse(message.data) as T });
  stream.addEventListener(channel, listener);
  return () => stream.removeEventListener(channel, listener);
}

export async function openUrl(url: string): Promise<void> {
  window.open(url, "_blank", "noopener,noreferrer");
}
