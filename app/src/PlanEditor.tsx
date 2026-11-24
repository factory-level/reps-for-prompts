import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";
import type { PlanItem, Routine, TrainingPlan } from "./snapshot";
import { titleCase } from "./snapshot";

type CatalogEntry = { id: string; kind: "lift" | "timed"; seconds: number; vision: boolean };
const DAYS = ["M", "T", "W", "T", "F", "S", "S"];
const DAY_NAMES = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
const uid = (p: string) => `${p}-${Math.random().toString(36).slice(2, 9)}`;

function move<T>(list: T[], i: number, by: number): T[] {
  const j = i + by;
  if (j < 0 || j >= list.length) return list;
  const out = [...list];
  [out[i], out[j]] = [out[j], out[i]];
  return out;
}

function RoutineBuilder({ routine, catalog, onChange, onRemove }: { routine: Routine; catalog: CatalogEntry[]; onChange: (r: Routine) => void; onRemove: () => void }) {
  const [custom, setCustom] = useState("");
  const set = (i: number, patch: Partial<PlanItem>) => onChange({ ...routine, items: routine.items.map((it, k) => (k === i ? { ...it, ...patch } : it)) });
  const add = (id: string) => {
    if (!id.trim()) return;
    const known = catalog.find((c) => c.id === id);
    const kind = known?.kind ?? "lift";
    const item: PlanItem = { exercise: id.trim().toLowerCase().replace(/\s+/g, "_"), label: titleCase(id.trim()), kind, sets: 3, reps: kind === "lift" ? 10 : 0, seconds: kind === "timed" ? known?.seconds || 30 : 0, defaultWeight: null, load: kind === "lift" ? "total" : "none" };
    onChange({ ...routine, items: [...routine.items, item] });
  };
  const sets = routine.items.reduce((n, i) => n + (Number(i.sets) || 0), 0);
  return (
    <section className="card">
      <div className="row" style={{ justifyContent: "space-between" }}>
        <label className="field">Routine name<input className="input" value={routine.name} onChange={(e) => onChange({ ...routine, name: e.target.value })} /></label>
        <span className="caption">{routine.items.length} exercises · {sets} planned sets</span>
        <button className="btn btn--small" onClick={onRemove}>Archive routine</button>
      </div>
      <table className="rows">
        <thead><tr><th>Exercise</th><th>Tracking</th><th>Sets</th><th>Reps / sec</th><th>Default lbs</th><th>Load</th><th /></tr></thead>
        <tbody>
          {routine.items.map((it, i) => {
            const vision = catalog.find((c) => c.id === it.exercise)?.vision;
            return (
              <tr key={i}>
                <td><input className="input" aria-label="Exercise name" value={it.label} onChange={(e) => set(i, { label: e.target.value })} /></td>
                <td><span className={`tag ${vision ? "tag--sky" : ""}`}>{vision ? "Camera" : "Manual"}</span></td>
                <td><input className="input num" aria-label="Sets" type="number" min={1} value={it.sets} onChange={(e) => set(i, { sets: Number(e.target.value) })} /></td>
                <td>{it.kind === "lift"
                  ? <input className="input num" aria-label="Reps" type="number" min={1} value={it.reps} onChange={(e) => set(i, { reps: Number(e.target.value) })} />
                  : <input className="input num" aria-label="Seconds" type="number" min={1} value={it.seconds} onChange={(e) => set(i, { seconds: Number(e.target.value) })} />}</td>
                <td>{it.kind === "lift" ? <input className="input num" aria-label="Default weight in lbs" type="number" min={0} step="any" value={it.defaultWeight ?? ""} onChange={(e) => set(i, { defaultWeight: e.target.value === "" ? null : Number(e.target.value) })} /> : "—"}</td>
                <td>{it.kind === "lift" ? (
                  <select className="input" aria-label="Load convention" value={it.load} onChange={(e) => set(i, { load: e.target.value as PlanItem["load"] })}>
                    <option value="total">Total</option><option value="per_dumbbell">Per dumbbell</option><option value="none">Bodyweight</option>
                  </select>) : "—"}</td>
                <td style={{ whiteSpace: "nowrap" }}>
                  <button className="btn btn--small" aria-label="Move up" onClick={() => onChange({ ...routine, items: move(routine.items, i, -1) })}>↑</button>{" "}
                  <button className="btn btn--small" aria-label="Move down" onClick={() => onChange({ ...routine, items: move(routine.items, i, 1) })}>↓</button>{" "}
                  <button className="btn btn--small" aria-label="Remove" onClick={() => onChange({ ...routine, items: routine.items.filter((_, k) => k !== i) })}>×</button>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
      <div className="row" style={{ marginTop: 10 }}>
        <label className="field">Add a camera-tracked exercise
          <select className="input" value="" onChange={(e) => add(e.target.value)}>
            <option value="">Choose…</option>
            {catalog.filter((c) => c.vision).map((c) => <option key={c.id} value={c.id}>{titleCase(c.id)}{c.kind === "timed" ? " (timed)" : ""}</option>)}
          </select>
        </label>
        <form className="row" onSubmit={(e) => { e.preventDefault(); add(custom); setCustom(""); }}>
          <label className="field">or a custom one (logged by hand)<input className="input" value={custom} placeholder="Goblet squat" onChange={(e) => setCustom(e.target.value)} /></label>
          <button className="btn" type="submit">Add</button>
        </form>
      </div>
    </section>
  );
}

export function PlanEditor() {
  const [plan, setPlan] = useState<TrainingPlan | null>(null);
  const [saved, setSaved] = useState<string>("");
  const [catalog, setCatalog] = useState<CatalogEntry[]>([]);
  const [open, setOpen] = useState<string | null>(null);
  const [message, setMessage] = useState<{ ok: boolean; text: string } | null>(null);
  useEffect(() => {
    void invoke<TrainingPlan>("get_plan").then((p) => { setPlan(p); setSaved(JSON.stringify(p)); setOpen(p.routines.find((r) => !r.archived)?.id ?? null); });
    void invoke<CatalogEntry[]>("catalog").then(setCatalog, () => {});
  }, []);
  if (!plan) return <div className="shell-body">Loading your plan…</div>;
  const dirty = JSON.stringify(plan) !== saved;
  const routines = plan.routines.filter((r) => !r.archived);
  const name = (id: string) => plan.routines.find((r) => r.id === id)?.name ?? "—";
  const edit = (p: Partial<TrainingPlan>) => { setPlan({ ...plan, ...p }); setMessage(null); };
  const save = () => invoke<TrainingPlan>("save_plan", { plan }).then(
    (p) => { setSaved(JSON.stringify(p)); setMessage({ ok: true, text: "Saved. Your next workout uses it." }); },
    (e) => setMessage({ ok: false, text: String(e) }));
  const addRoutine = () => {
    const r: Routine = { id: uid("routine"), name: `Routine ${plan.routines.length + 1}`, archived: false, items: [] };
    edit({ routines: [...plan.routines, r], cycle: [...plan.cycle, { slotId: uid("slot"), routineId: r.id }] });
    setOpen(r.id);
  };
  const archive = (id: string) => {
    const cycle = plan.cycle.filter((s) => s.routineId !== id);
    edit({ routines: plan.routines.map((r) => (r.id === id ? { ...r, archived: true } : r)), cycle, nextSlot: cycle.some((s) => s.slotId === plan.nextSlot) ? plan.nextSlot : cycle[0]?.slotId ?? "" });
    setOpen(routines.find((r) => r.id !== id)?.id ?? null);
  };
  const removeSlot = (i: number) => {
    const cycle = plan.cycle.filter((_, k) => k !== i);
    const next = plan.cycle[i].slotId === plan.nextSlot ? cycle[i % Math.max(cycle.length, 1)]?.slotId ?? "" : plan.nextSlot;
    edit({ cycle, nextSlot: next });
  };
  const chooseNext = (slotId: string) => {
    if (window.confirm?.("Make this the next routine? Nothing is marked complete.") === false) return;
    edit({ nextSlot: slotId });
  };
  const current = plan.routines.find((r) => r.id === open);
  return (
    <div className="shell-body">
      <div className="row" style={{ justifyContent: "flex-end" }}>
        {message && <span role={message.ok ? "status" : "alert"}>{message.text}</span>}
        <button className="btn btn--cta" disabled={!dirty} onClick={() => void save()}>{dirty ? "Save plan" : "Saved"}</button>
      </div>
      <div className="plan-grid">
        <section className="card">
          <h2>Ordered cycle</h2>
          <p className="caption">Workouts run in this order and wrap around. A missed day never shifts or doubles anything.</p>
          <ol className="cycle">
            {plan.cycle.map((slot, i) => (
              <li key={slot.slotId}>
                <span className="n">{i + 1}</span>
                <select className="input" aria-label={`Routine in slot ${i + 1}`} value={slot.routineId} onChange={(e) => edit({ cycle: plan.cycle.map((s, k) => (k === i ? { ...s, routineId: e.target.value } : s)) })}>
                  {routines.map((r) => <option key={r.id} value={r.id}>{r.name}</option>)}
                </select>
                {slot.slotId === plan.nextSlot ? <span className="tag tag--go">Next</span> : <button className="btn btn--small" onClick={() => chooseNext(slot.slotId)}>Make next</button>}
                <button className="btn btn--small" aria-label="Move up" onClick={() => edit({ cycle: move(plan.cycle, i, -1) })}>↑</button>
                <button className="btn btn--small" aria-label="Move down" onClick={() => edit({ cycle: move(plan.cycle, i, 1) })}>↓</button>
                <button className="btn btn--small" aria-label="Remove from cycle" disabled={plan.cycle.length === 1} onClick={() => removeSlot(i)}>×</button>
              </li>
            ))}
          </ol>
          <button className="btn btn--small" disabled={!routines.length} onClick={() => edit({ cycle: [...plan.cycle, { slotId: uid("slot"), routineId: routines[0].id }] })}>+ Add to cycle</button>
          <p className="caption">Next up: {name(plan.cycle.find((s) => s.slotId === plan.nextSlot)?.routineId ?? "")}</p>
        </section>
        <section className="card">
          <h2>Weekly commitment</h2>
          <div className="days" role="group" aria-label="Training days">
            {DAYS.map((d, i) => {
              const on = plan.weekdays.includes(i + 1);
              return <button key={i} className="btn" aria-label={DAY_NAMES[i]} aria-pressed={on} onClick={() => edit({ weekdays: on ? plan.weekdays.filter((x) => x !== i + 1) : [...plan.weekdays, i + 1].sort() })}>{d}</button>;
            })}
          </div>
          <div className="row" style={{ marginTop: 10 }}>
            <label className="field">Target workouts / week<input className="input num" type="number" min={1} max={14} value={plan.weeklyTarget} onChange={(e) => edit({ weeklyTarget: Number(e.target.value) })} /></label>
          </div>
          {plan.weeklyTarget !== plan.weekdays.length && <p className="caption">Target ({plan.weeklyTarget}) differs from your training days ({plan.weekdays.length}). That's allowed.</p>}
          <h3 style={{ marginTop: 14 }}>Start reminders</h3>
          <div className="row">
            {plan.startTimes.map((t, i) => (
              <label key={i} className="field">Reminder {i + 1}<input className="input" type="time" value={t} onChange={(e) => edit({ startTimes: plan.startTimes.map((x, k) => (k === i ? e.target.value : x)) })} /></label>
            ))}
          </div>
          <h3 style={{ marginTop: 14 }}>Workout timing</h3>
          <div className="row">
            <label className="field">Coding interval (min)<input className="input num" type="number" min={1} max={240} value={plan.codingMinutes} onChange={(e) => edit({ codingMinutes: Number(e.target.value) })} /></label>
            <label className="field">Snooze (min)<input className="input num" type="number" min={1} max={60} value={plan.snoozeMinutes} onChange={(e) => edit({ snoozeMinutes: Number(e.target.value) })} /></label>
            <span className="field">Unfinished-set repeat<span className="caption">every 5 seconds</span></span>
          </div>
        </section>
      </div>
      <section>
        <div className="routine-tabs" role="tablist" aria-label="Routines">
          <h2 style={{ marginRight: 8 }}>Routines</h2>
          {routines.map((r) => <button key={r.id} role="tab" className="btn btn--small" aria-selected={open === r.id} onClick={() => setOpen(r.id)}>{r.name}</button>)}
          <button className="btn btn--small btn--sky" onClick={addRoutine}>+ New routine</button>
        </div>
      </section>
      {current && <RoutineBuilder key={current.id} routine={current} catalog={catalog}
        onChange={(r) => edit({ routines: plan.routines.map((x) => (x.id === r.id ? r : x)) })}
        onRemove={() => archive(current.id)} />}
    </div>
  );
}
