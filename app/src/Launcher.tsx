import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useState } from "react";
import type { AppState, Snapshot } from "./snapshot";
import { mmss, type Vision } from "./Screen";
import { PlanEditor } from "./PlanEditor";
import { History } from "./History";

export type View = "home" | "plan" | "history" | "settings";

/** The design system's two marks: a dumbbell is WORKOUT, a terminal is CODE. */
export const Dumbbell = () => (
  <svg viewBox="0 0 28 24" fill="none" aria-hidden="true"><path d="M8 10h12v4H8z" fill="var(--wood)" /><g fill="var(--ivory)"><rect x="4" y="5" width="5" height="14" rx="1.5" /><rect x="19" y="5" width="5" height="14" rx="1.5" /><path d="M1 9h3v6H1zM24 9h3v6h-3z" /></g></svg>
);
export const Terminal = () => (
  <svg viewBox="0 0 28 24" fill="none" aria-hidden="true"><rect x="1" y="2" width="26" height="20" rx="3" fill="var(--ivory)" /><path d="m7 8 4 4-4 4m8 0h6" strokeLinecap="round" strokeLinejoin="round" /></svg>
);

function statusLine(snapshot: Snapshot | null, app: AppState | null): string {
  if (!snapshot || !app) return "Connecting…";
  if (snapshot.paused) return `Paused · ${app.routine?.name ?? ""}`;
  switch (snapshot.phase) {
    case "IDLE": return app.setup ? "Workout monitor is up · press Start session" : `Ready · ${app.week.done} of ${app.week.target} this week`;
    case "CODING": return `${app.routine?.name} · coding · next set in ${mmss(snapshot.remainingSeconds)}`;
    case "EXERCISE_REQUIRED": return "A set is due";
    case "WORKOUT_ACTIVE": return "Set under way";
    case "WEIGHT_CONFIRMATION": return "Log your set";
    default: return "Set logged";
  }
}

function Home({ snapshot, app, go }: { snapshot: Snapshot | null; app: AppState | null; go: (v: View) => void }) {
  const [error, setError] = useState<string | null>(null);
  const live = !!snapshot && snapshot.phase !== "IDLE";
  const act = (name: string) => invoke(name).then(() => setError(null), (e) => setError(String(e)));
  return (
    <div className="launcher">
      <header className="launcher-top">
        <div>
          <h1 className="wordmark"><span className="sticker sticker--code">Reps for</span><span className="sticker sticker--workout">Prompts</span></h1>
          <p className="tagline">Small workouts. A little more often.</p>
        </div>
        <button className="btn cog" aria-label="Settings" title="Settings" onClick={() => go("settings")}>⚙</button>
      </header>
      <main className="actions">
        <button className="action action--start" onClick={() => void act("prepare_workout")}>
          <Dumbbell />
          <span className="action-label">{live ? "Resume workout" : "Start workout"}</span>
          <span className="action-sub">{live ? app?.routine?.name : app?.next ? `Next: ${app.next.name}` : "Set up a plan first"}</span>
        </button>
        <button className="action action--config" onClick={() => go("plan")}>
          <Terminal />
          <span className="action-label">Configure workout</span>
          <span className="action-sub">Plan your training</span>
        </button>
      </main>
      <footer className="launcher-status" role="status">
        <span className={`dot${live ? " dot--busy" : ""}`} aria-hidden />
        <span>{statusLine(snapshot, app)}</span>
        {app?.setup && !live && <span className="setup-bar">
          <button className="btn btn--cta" onClick={() => void act("start_session")}>Start session</button>
          <button className="btn" onClick={() => void act("cancel_setup")}>Cancel</button>
        </span>}
        {live && <span className="setup-bar">
          {snapshot!.paused ? <button className="btn" onClick={() => void act("resume")}>Resume</button>
            : snapshot!.phase === "CODING" && <button className="btn" onClick={() => void act("start_set")}>Start next set now</button>}
          <button className="btn btn--ghost" onClick={() => void act("end_workout")}>End workout</button>
        </span>}
        {error && <span role="alert">{error}</span>}
      </footer>
    </div>
  );
}

function Settings({ app, vision }: { app: AppState | null; vision?: Vision }) {
  return (
    <div className="shell-body">
      <section className="card">
        <h2>Workout monitor</h2>
        <p>The monitor goes on the screen you last dragged it to, fullscreen unless you turned that off (F11 or its button).</p>
        <div className="row">
          <button className="btn btn--sky" onClick={() => void invoke("show_gym")}>Show workout monitor</button>
          <button className="btn" onClick={() => void invoke("show_gym", { reset: true })}>Reset placement</button>
        </div>
      </section>
      <section className="card">
        <h2>Camera</h2>
        <p>{vision?.state === "unavailable" ? `Unavailable: ${vision.reason ?? "unknown"}. Sets can still be finished by hand.` : vision?.state === "available" ? "Vision tracking is available." : "Starting the local vision hub…"}</p>
        <p className="caption">Video never leaves this machine. Camera setup: <code>rfp camera list</code>, <code>rfp camera set</code>.</p>
      </section>
      <section className="card">
        <h2>Mode</h2>
        <p>{app?.debug ? "Debug mode: nothing is saved to your workouts." : "Workout mode."} Switch with <code>rfp mode debug|workout</code>.</p>
      </section>
    </div>
  );
}

const TITLES: Record<View, string> = { home: "", plan: "Training plan", history: "Progress & history", settings: "Settings" };

export function Launcher({ snapshot, app, vision }: { snapshot: Snapshot | null; app: AppState | null; vision?: Vision }) {
  const [view, setView] = useState<View>("home");
  useEffect(() => {
    let unlisten: (() => void) | undefined; let active = true;
    void listen<View>("navigate", (e) => setView(e.payload)).then((u) => { if (active) unlisten = u; else u(); });
    return () => { active = false; unlisten?.(); };
  }, []);
  if (view === "home") return <Home snapshot={snapshot} app={app} go={setView} />;
  return (
    <div className="shell">
      <header className="shell-bar">
        <button className="btn" onClick={() => setView("home")}>← Home</button>
        <h1>{TITLES[view]}</h1>
        <nav className="tabs" aria-label="Sections">
          {(["plan", "history", "settings"] as View[]).map((v) => (
            <button key={v} className="btn btn--small" aria-selected={view === v} onClick={() => setView(v)}>{TITLES[v]}</button>
          ))}
        </nav>
      </header>
      {view === "plan" ? <PlanEditor /> : view === "history" ? <History /> : <Settings app={app} vision={vision} />}
    </div>
  );
}
