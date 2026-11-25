import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useState } from "react";
import type { AppState } from "./snapshot";

/** The backend's app state: fetched once, then kept current by `app-state` events. */
export function useAppState(): AppState | null {
  const [state, setState] = useState<AppState | null>(null);
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    invoke<AppState>("get_app_state").then((s) => { if (!cancelled) setState(s); }, () => {});
    listen<AppState>("app-state", (e) => setState(e.payload)).then((u) => { if (cancelled) u(); else unlisten = u; });
    return () => { cancelled = true; unlisten?.(); };
  }, []);
  return state;
}
