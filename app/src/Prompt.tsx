import { invoke } from "@tauri-apps/api/core";
import { useEffect, useRef, useState } from "react";
import type { AppState, Snapshot } from "./snapshot";
import { titleCase } from "./snapshot";
import type { Vision } from "./Screen";

const LOAD_LABEL = { total: "lbs total", per_dumbbell: "lbs / dumbbell", none: "" } as const;

/** Run a command; keep the error on screen instead of pretending it worked. */
function useAction() {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const run = async (name: string, args?: Record<string, unknown>) => {
    if (busy) return false;
    setBusy(true); setError(null);
    try { await invoke(name, args); return true; } catch (e) { setError(String(e)); return false; } finally { setBusy(false); }
  };
  return { busy, error, run };
}

function Head({ title, tone = "" }: { title: string; tone?: string }) {
  return (
    <div className="prompt-head" data-tauri-drag-region>
      <h1 className={tone} data-tauri-drag-region>{title}</h1>
      <button className="close" aria-label="Not now" title="Not now (Esc)" onClick={() => void invoke("dismiss_prompt")}>×</button>
    </div>
  );
}

/** P03: actual reps and lbs. The draft lives here; the window is hidden, never destroyed, so it survives. */
function Confirm({ snapshot, app }: { snapshot: Snapshot; app: AppState }) {
  const rx = snapshot.prescription!;
  const item = snapshot.day?.items.find((i) => i.name === rx.exercise);
  const label = item?.label ?? titleCase(rx.exercise);
  const detected = Math.floor(snapshot.progress?.value ?? rx.targetReps);
  const initial = app.lastWeight ?? (rx.defaultWeight > 0 ? rx.defaultWeight : null);
  const [reps, setReps] = useState(String(detected || rx.targetReps));
  const [weight, setWeight] = useState(initial === null ? "" : String(initial));
  const [invalid, setInvalid] = useState<string | null>(null);
  const weightRef = useRef<HTMLInputElement>(null);
  const { busy, error, run } = useAction();
  const last = (snapshot.day?.setsTotal ?? 0) - (snapshot.day?.setsDone ?? 0) <= 1;
  const unit = LOAD_LABEL[app.load] || "lbs";
  useEffect(() => { weightRef.current?.focus(); weightRef.current?.select(); }, []);
  const save = () => {
    const w = Number(weight), r = Number(reps);
    if (!weight.trim() || !Number.isFinite(w) || w < 0) return setInvalid("Enter the weight in lbs (0 or more).");
    if (!Number.isInteger(r) || r < 1) return setInvalid("Reps must be a whole number, 1 or more.");
    setInvalid(null);
    void run("confirm_set", { weight: w, reps: r });
  };
  return (
    <form className="prompt" onSubmit={(e) => { e.preventDefault(); save(); }}>
      <Head title="Nice work" tone="go" />
      <div className="prompt-what">{label} · set {Math.min((item?.done ?? 0) + 1, item?.total ?? 1)} of {item?.total ?? 1}</div>
      <div className="confirm-grid">
        <label htmlFor="reps">Reps</label>
        <span><input id="reps" className="input input--big" inputMode="numeric" value={reps} onChange={(e) => setReps(e.target.value)} disabled={busy} /></span>
        <label htmlFor="weight">Weight</label>
        <span>
          <input id="weight" ref={weightRef} className="input input--big" inputMode="decimal" value={weight} placeholder="0" onChange={(e) => setWeight(e.target.value)} disabled={busy} />
          <span className="unit">{unit}</span>
        </span>
      </div>
      <span className="caption">{app.lastWeight !== null ? `Last time: ${app.lastWeight} lbs` : "No history yet for this exercise."} Correct the reps if they're off.</span>
      {(invalid || error) && <span role="alert">{invalid ?? error}</span>}
      <div className="prompt-actions">
        <button type="submit" className="btn btn--cta" disabled={busy}>{busy ? "Saving…" : last ? "Save & finish workout" : "Save set & back to code"}</button>
        <button type="button" className="btn btn--small" onClick={() => void invoke("show_main", { view: "history" })}>View history</button>
        <span className="caption">Enter to save · Esc for later</span>
      </div>
    </form>
  );
}

export function Prompt({ snapshot, app, vision }: { snapshot: Snapshot | null; app: AppState | null; vision?: Vision }) {
  const { busy, error, run } = useAction();
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") void invoke("dismiss_prompt"); };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
  if (!snapshot || !app || !app.prompt) return <div className="prompt"><Head title="All clear" tone="code" /><p>Nothing due right now.</p></div>;
  const rx = snapshot.prescription;
  const item = snapshot.day?.items.find((i) => i.name === rx?.exercise);
  const label = item?.label ?? (rx ? titleCase(rx.exercise) : "");
  const target = rx?.kind === "REP" ? `${rx.targetReps} reps` : `${rx?.targetSeconds ?? 0} sec`;
  const setOf = item ? `Set ${Math.min(item.done + 1, item.total)} of ${item.total}` : "";
  const alert = error && <span role="alert">{error}</span>;

  switch (app.prompt) {
    case "start":
      return (
        <div className="prompt">
          <Head title="Time for your workout" tone="code" />
          <div className="prompt-what">Next: {app.next?.name ?? "—"}</div>
          <span>{app.week.done} of {app.week.target} workouts this week</span>
          {alert}
          <div className="prompt-actions">
            <button className="btn btn--cta" autoFocus disabled={busy} onClick={() => void run("prepare_workout").then((ok) => ok && invoke("dismiss_prompt"))}>Start workout</button>
            <button className="btn" onClick={() => void invoke("dismiss_prompt")}>Not now</button>
          </div>
        </div>
      );
    case "confirm":
      return rx ? <Confirm key={`${rx.exercise}-${snapshot.day?.setsDone}`} snapshot={snapshot} app={app} /> : null;
    case "exercising": {
      const manual = vision?.state === "unavailable";
      return (
        <div className="prompt">
          <Head title="Set under way" />
          <div className="prompt-what">{label} · {setOf}</div>
          <span>{manual ? "Camera unavailable. Do the set, then finish it here." : `Camera is counting · ${Math.floor(snapshot.progress?.value ?? 0)} of ${target}`}</span>
          {alert}
          <div className="prompt-actions">
            <button className="btn btn--cta" disabled={busy} onClick={() => void run("manual_finish")}>I finished this set</button>
          </div>
        </div>
      );
    }
    default:
      return (
        <div className="prompt">
          <Head title="Time to move" />
          <div className="prompt-what">{label}</div>
          <span>{setOf} · {target}</span>
          {alert}
          <div className="prompt-actions">
            <button className="btn btn--cta" autoFocus disabled={busy} onClick={() => void run("start_set")}>Start set</button>
            <button className="btn" disabled={busy} onClick={() => void run("snooze")}>Snooze {app.snoozeMinutes}m</button>
            <button className="btn" disabled={busy} onClick={() => void run("pause")}>Pause</button>
          </div>
        </div>
      );
  }
}
