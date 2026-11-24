import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useState } from "react";
import "./retro.css";
import "./ui.css";
import { Screen, type Vision } from "./Screen";
import { Launcher } from "./Launcher";
import { Prompt } from "./Prompt";
import { PassivePreview } from "./PassivePreview";
import { useSnapshot } from "./useSnapshot";
import { useAppState } from "./useAppState";

type Display = { view: "screen" | "camera" | "video"; previewOnly: boolean; lastError: string | null; vision?: Vision };
export type Role = "main" | "gym" | "prompt";
const param = new URLSearchParams(window.location.search).get("window");
const role: Role = param === "gym" ? "gym" : param === "prompt" ? "prompt" : "main";
// Gym TV: F11 toggles fullscreen (remembered across launches).
if (role === "gym") window.addEventListener("keydown", (e) => { if (e.key === "F11") { e.preventDefault(); void invoke("toggle_fullscreen"); } });
document.documentElement.classList.add(role === "gym" ? "gym-root" : "desk-root");

export default function App({ forceRole }: { forceRole?: Role } = {}) {
  const which = forceRole ?? role;
  const snapshot = useSnapshot();
  const app = useAppState();
  const [display, setDisplay] = useState<Display>({ view: "screen", previewOnly: false, lastError: null });
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let active = true; let unlisten: (() => void) | undefined;
    void listen<Display>("display-state", (e) => { if (active) setDisplay(e.payload); }).then((fn) => { if (active) unlisten = fn; else fn(); });
    void invoke<Display>("get_display_state").then((d) => { if (active) setDisplay(d); }).catch((e) => { if (active) setError(String(e)); });
    return () => { active = false; unlisten?.(); };
  }, []);
  if (error) return <p role="alert">RFP is unavailable: {error}</p>;
  if (which === "gym") {
    return <main className="gym">{snapshot ? <Screen snapshot={snapshot} app={app} debug={app?.debug} vision={display.vision} /> : <p className="small">Connecting to RFP…</p>}</main>;
  }
  if (which === "prompt") return <main className="desk prompt-root"><Prompt snapshot={snapshot} app={app} vision={display.vision} /></main>;
  return (
    <main className="desk">
      {display.view !== "screen"
        ? <PassivePreview kind={display.view} error={display.lastError} vision={display.vision} />
        : <Launcher snapshot={snapshot} app={app} vision={display.vision} />}
    </main>
  );
}
