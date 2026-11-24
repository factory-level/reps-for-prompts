import { invoke } from "@tauri-apps/api/core";
import { useEffect, useMemo, useState } from "react";
import type { SetRow, WorkoutRow } from "./snapshot";
import { titleCase } from "./snapshot";

const iso = (d: Date) => `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
const DASH = "—";
const fmtSet = (s: SetRow) => (s.kind === "continuous" ? `${Math.round(s.seconds)} s` : `${s.weight} lbs × ${s.reps}`);
const how = (s: SetRow) => (s.verified ? "camera" : "by hand");

function SetsTable({ sets }: { sets: SetRow[] }) {
  if (!sets.length) return <p className="caption">No sets recorded.</p>;
  return (
    <table className="sets">
      <thead><tr><th>#</th><th>Exercise</th><th>Result</th><th>Tracked</th></tr></thead>
      <tbody>{sets.map((s, i) => <tr key={s.id}><td>{i + 1}</td><td>{titleCase(s.exercise)}</td><td>{fmtSet(s)}</td><td>{how(s)}</td></tr>)}</tbody>
    </table>
  );
}

/** H01: month calendar with completed / partial markers and the selected day's sessions. */
function Calendar() {
  const [month, setMonth] = useState(() => { const d = new Date(); return new Date(d.getFullYear(), d.getMonth(), 1); });
  const [picked, setPicked] = useState(iso(new Date()));
  const [data, setData] = useState<{ workouts: WorkoutRow[]; sets: SetRow[] }>({ workouts: [], sets: [] });
  const [list, setList] = useState(false);
  const first = iso(month), last = iso(new Date(month.getFullYear(), month.getMonth() + 1, 0));
  useEffect(() => { void invoke<typeof data>("history", { from: first, to: last }).then(setData, () => {}); }, [first, last]);
  const status = (date: string) => {
    const w = data.workouts.filter((x) => x.date === date);
    if (w.some((x) => x.status === "completed")) return "completed";
    if (w.some((x) => x.status === "partial") || data.sets.some((s) => s.date === date)) return "partial";
    return "";
  };
  const lead = (month.getDay() + 6) % 7;
  const days = new Date(month.getFullYear(), month.getMonth() + 1, 0).getDate();
  const shift = (n: number) => setMonth(new Date(month.getFullYear(), month.getMonth() + n, 1));
  const dayWorkouts = (date: string) => data.workouts.filter((w) => w.date === date && w.status !== "canceled");
  const daySets = (date: string, id?: string) => data.sets.filter((s) => (id ? s.workoutId === id : s.date === date && !s.workoutId));
  const dates = [...new Set([...data.workouts.map((w) => w.date), ...data.sets.map((s) => s.date)])].sort().reverse();
  const Day = ({ date }: { date: string }) => {
    const w = dayWorkouts(date), loose = daySets(date);
    if (!w.length && !loose.length) return <p className="caption">Nothing recorded on {date}.</p>;
    return <>{w.map((x) => <div key={x.id} className="card" style={{ marginBottom: 10 }}>
      <div className="row" style={{ justifyContent: "space-between" }}><h3>{x.routine}</h3><span className={`tag ${x.status === "completed" ? "tag--go" : ""}`}>{x.status}</span></div>
      <SetsTable sets={daySets(date, x.id)} />
    </div>)}{loose.length > 0 && <div className="card"><h3>Other sets</h3><SetsTable sets={loose} /></div>}</>;
  };
  return (
    <div className="shell-body">
      <div className="row">
        <div className="tabs" role="tablist">
          <button role="tab" className="btn btn--small" aria-selected={!list} onClick={() => setList(false)}>Calendar</button>
          <button role="tab" className="btn btn--small" aria-selected={list} onClick={() => setList(true)}>List</button>
        </div>
        <button className="btn btn--small" aria-label="Previous month" onClick={() => shift(-1)}>‹</button>
        <strong>{month.toLocaleString(undefined, { month: "long", year: "numeric" })}</strong>
        <button className="btn btn--small" aria-label="Next month" onClick={() => shift(1)}>›</button>
        <button className="btn btn--small" onClick={() => { const d = new Date(); setMonth(new Date(d.getFullYear(), d.getMonth(), 1)); setPicked(iso(d)); }}>Today</button>
      </div>
      {list ? (
        dates.length ? dates.map((d) => <section key={d}><h2 style={{ fontSize: 20, margin: "6px 0" }}>{d}</h2><Day date={d} /></section>) : <p className="caption">No workouts this month yet.</p>
      ) : (
        <div className="split">
          <section className="card">
            <div className="month">
              {["M", "T", "W", "T", "F", "S", "S"].map((d, i) => <span key={i} className="wd">{d}</span>)}
              {Array.from({ length: lead }, (_, i) => <span key={`x${i}`} />)}
              {Array.from({ length: days }, (_, i) => {
                const date = iso(new Date(month.getFullYear(), month.getMonth(), i + 1));
                const s = status(date);
                return <button key={date} className={`day ${s ? `day--${s}` : ""}`} aria-pressed={picked === date} aria-label={`${date}${s ? `, ${s}` : ""}`} onClick={() => setPicked(date)}>
                  {i + 1}<span className="mark" aria-hidden>{s === "completed" ? "●" : s === "partial" ? "◐" : " "}</span>
                </button>;
              })}
            </div>
            <p className="caption">● completed routine · ◐ partial or extra sets</p>
          </section>
          <section><h2 style={{ fontSize: 22, marginBottom: 8 }}>{new Date(`${picked}T12:00`).toLocaleDateString(undefined, { weekday: "long", month: "long", day: "numeric" })}</h2><Day date={picked} /></section>
        </div>
      )}
    </div>
  );
}

/** Estimated 1RM (Epley) for 2–10 reps; 1 rep is the actual weight. Not a recommendation. */
export const e1rm = (w: number, reps: number) => (reps === 1 ? w : reps >= 2 && reps <= 10 ? w * (1 + reps / 30) : null);

function Graph({ points }: { points: { date: string; value: number }[] }) {
  if (points.length < 2) return <p className="caption">Two or more sessions with a weight make a graph.</p>;
  const W = 640, H = 220, P = 36;
  const t = points.map((p) => new Date(`${p.date}T12:00`).getTime());
  const [t0, t1] = [Math.min(...t), Math.max(...t)];
  const vs = points.map((p) => p.value);
  const [v0, v1] = [Math.min(...vs), Math.max(...vs)];
  const x = (i: number) => P + ((t[i] - t0) / Math.max(t1 - t0, 1)) * (W - 2 * P);
  const y = (v: number) => H - P - ((v - v0) / Math.max(v1 - v0, 1)) * (H - 2 * P);
  return (
    <svg className="chart" viewBox={`0 0 ${W} ${H}`} role="img" aria-label="Max weight per session over time">
      <line x1={P} y1={H - P} x2={W - P} y2={H - P} stroke="var(--ink)" strokeWidth={2} />
      <line x1={P} y1={P} x2={P} y2={H - P} stroke="var(--ink)" strokeWidth={2} />
      <text x={P - 6} y={y(v1) + 4} textAnchor="end" fontSize={12} fill="var(--ink)">{v1}</text>
      <text x={P - 6} y={y(v0) + 4} textAnchor="end" fontSize={12} fill="var(--ink)">{v0}</text>
      <text x={P} y={H - 12} fontSize={12} fill="var(--ink)">{points[0].date}</text>
      <text x={W - P} y={H - 12} textAnchor="end" fontSize={12} fill="var(--ink)">{points[points.length - 1].date}</text>
      <polyline fill="none" stroke="var(--orange)" strokeWidth={3} points={points.map((p, i) => `${x(i)},${y(p.value)}`).join(" ")} />
      {points.map((p, i) => <circle key={i} cx={x(i)} cy={y(p.value)} r={5} fill="var(--orange)" stroke="var(--ink)" strokeWidth={2}><title>{p.date}: {p.value} lbs</title></circle>)}
    </svg>
  );
}

/** H02: one exercise's History | Graph | Records | Stats from confirmed sets only. */
function Exercise() {
  const [names, setNames] = useState<string[]>([]);
  const [name, setName] = useState<string | null>(null);
  const [sets, setSets] = useState<SetRow[]>([]);
  const [tab, setTab] = useState<"history" | "graph" | "records" | "stats">("history");
  useEffect(() => { void invoke<{ exercises: string[] }>("exercise_history", { exercise: null }).then((r) => { setNames(r.exercises); setName((n) => n ?? r.exercises[0] ?? null); }, () => {}); }, []);
  useEffect(() => { if (name) void invoke<{ sets: SetRow[] }>("exercise_history", { exercise: name }).then((r) => setSets(r.sets), () => {}); }, [name]);
  const lifts = sets.filter((s) => s.kind === "rep");
  const byDate = useMemo(() => {
    const m = new Map<string, SetRow[]>();
    for (const s of sets) m.set(s.date, [...(m.get(s.date) ?? []), s]);
    return [...m.entries()];
  }, [sets]);
  const points = byDate.map(([date, ss]) => ({ date, value: Math.max(...ss.filter((s) => s.kind === "rep").map((s) => s.weight)) })).filter((p) => Number.isFinite(p.value) && p.value > 0);
  const records = useMemo(() => {
    const best = new Map<number, SetRow>();
    for (const s of lifts) { const b = best.get(s.reps); if (!b || s.weight > b.weight) best.set(s.reps, s); } // ties keep the first: not a new best
    return [...best.entries()].sort((a, b) => a[0] - b[0]);
  }, [lifts]);
  const bestE1 = lifts.reduce<{ v: number; s: SetRow } | null>((b, s) => { const v = e1rm(s.weight, s.reps); return v !== null && s.weight > 0 && (!b || v > b.v) ? { v, s } : b; }, null);
  const volume = lifts.filter((s) => s.weight > 0).reduce((n, s) => n + s.weight * s.reps, 0);
  const timed = sets.filter((s) => s.kind === "continuous");
  if (!names.length) return <div className="shell-body"><p className="caption">No confirmed sets yet. Finish a set and it shows up here.</p></div>;
  return (
    <div className="shell-body split">
      <nav className="ex-list card" aria-label="Exercises">
        {names.map((n) => <button key={n} className="btn btn--small" aria-pressed={n === name} onClick={() => setName(n)}>{titleCase(n)}</button>)}
      </nav>
      <section className="card">
        <h2>{name ? titleCase(name) : DASH}</h2>
        <div className="tabs" role="tablist" style={{ margin: "8px 0 12px" }}>
          {(["history", "graph", "records", "stats"] as const).map((t) => <button key={t} role="tab" className="btn btn--small" aria-selected={tab === t} onClick={() => setTab(t)}>{titleCase(t)}</button>)}
        </div>
        {tab === "history" && [...byDate].reverse().map(([date, ss]) => (
          <div key={date} style={{ marginBottom: 10 }}><strong>{date}</strong>
            <table className="sets"><tbody>{ss.map((s, i) => <tr key={s.id}><td>{i + 1}</td><td>{fmtSet(s)}</td><td>{how(s)}</td><td>{records.some(([, r]) => r.id === s.id) ? <span className="tag tag--go">PR</span> : ""}</td></tr>)}</tbody></table>
          </div>))}
        {tab === "graph" && <>
          <p className="caption">Max weight per session, on actual workout dates. Missing weights are left out, never zero.</p>
          <Graph points={points} />
          <table className="sets"><thead><tr><th>Date</th><th>Max lbs</th></tr></thead><tbody>{points.map((p) => <tr key={p.date}><td>{p.date}</td><td>{p.value}</td></tr>)}</tbody></table>
        </>}
        {tab === "records" && <>
          <table className="sets"><thead><tr><th>Reps</th><th>Best weight</th><th>Date</th></tr></thead>
            <tbody>{records.length ? records.map(([reps, s]) => <tr key={reps}><td>{reps}</td><td>{s.weight} lbs</td><td>{s.date}</td></tr>) : <tr><td colSpan={3}>{DASH}</td></tr>}</tbody></table>
          <p>Estimated 1RM: <strong>{bestE1 ? `${bestE1.v.toFixed(1)} lbs` : DASH}</strong> <span className="caption">{bestE1 ? `Epley estimate from ${bestE1.s.weight} × ${bestE1.s.reps} on ${bestE1.s.date}. An estimate, not a target.` : ""}</span></p>
          {timed.length > 0 && <p>Longest hold: <strong>{Math.max(...timed.map((s) => s.seconds))} s</strong></p>}
        </>}
        {tab === "stats" && <div className="stat-grid">
          <div className="stat">Sessions<b>{byDate.length}</b></div>
          <div className="stat">Sets<b>{sets.length}</b></div>
          <div className="stat">Total reps<b>{lifts.length ? lifts.reduce((n, s) => n + s.reps, 0) : DASH}</b></div>
          <div className="stat">Max reps<b>{lifts.length ? Math.max(...lifts.map((s) => s.reps)) : DASH}</b></div>
          <div className="stat">Max weight<b>{points.length ? `${Math.max(...points.map((p) => p.value))}` : DASH}</b></div>
          <div className="stat">Volume (lb·reps)<b>{volume ? volume.toLocaleString() : DASH}</b></div>
          <div className="stat">Total time<b>{timed.length ? `${Math.round(timed.reduce((n, s) => n + s.seconds, 0))} s` : DASH}</b></div>
          <div className="stat">First / last<b style={{ fontSize: 16 }}>{sets.length ? `${sets[0].date} → ${sets[sets.length - 1].date}` : DASH}</b></div>
        </div>}
      </section>
    </div>
  );
}

type Week = { start: string; end: string; target: number; done: number; partial: number; sets: number; reps: number; days: { date: string; status: string }[]; heat: Record<string, number>; next: string | null };
const MARK: Record<string, string> = { completed: "●", partial: "◐", active: "◔", planned: "·", rest: "" };

/** H03: this week against the commitment, no debt, no carry-over. */
function WeekView() {
  const [start, setStart] = useState<string | null>(null);
  const [w, setW] = useState<Week | null>(null);
  useEffect(() => { void invoke<Week>("week", { start }).then(setW, () => {}); }, [start]);
  if (!w) return <div className="shell-body">Loading…</div>;
  const pct = Math.round((w.done / Math.max(w.target, 1)) * 100);
  const shift = (n: number) => { const d = new Date(`${w.start}T12:00`); d.setDate(d.getDate() + 7 * n); setStart(iso(d)); };
  const heatDays = Array.from({ length: 364 }, (_, i) => { const d = new Date(); d.setDate(d.getDate() - 363 + i); return iso(d); });
  const max = Math.max(1, ...Object.values(w.heat));
  return (
    <div className="shell-body">
      <section className="panel">
        <div className="row" style={{ justifyContent: "space-between" }}>
          <span className="eyebrow">Week of {w.start} – {w.end}</span>
          <span><button className="btn btn--small" aria-label="Previous week" onClick={() => shift(-1)}>‹</button> <button className="btn btn--small" aria-label="Next week" onClick={() => shift(1)}>›</button></span>
        </div>
        <h2 style={{ fontSize: 34, margin: "6px 0" }}>{w.done} of {w.target} routines · {pct}%</h2>
        <div className="bar" role="progressbar" aria-valuemin={0} aria-valuemax={w.target} aria-valuenow={w.done}><span style={{ width: `${Math.min(pct, 100)}%` }} /></div>
        <div className="week-days" style={{ marginTop: 12 }}>
          {w.days.map((d, i) => <div key={d.date}><div><b>{["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][i]}</b></div><div aria-label={d.status} style={{ fontSize: 22 }}>{MARK[d.status] ?? ""}</div></div>)}
        </div>
        <p style={{ marginBottom: 0 }}>Sets: <b>{w.sets}</b> · Reps: <b>{w.reps}</b> · Partial routines: <b>{w.partial}</b> · Next: <b>{w.next ?? DASH}</b></p>
      </section>
      <section className="card">
        <h2 style={{ fontSize: 22, marginBottom: 8 }}>The last year</h2>
        <div className="heat" role="img" aria-label="Sets recorded per day over the last year">
          {heatDays.map((d) => { const n = w.heat[d] ?? 0; const level = n ? Math.min(4, Math.ceil((n / max) * 4)) : 0; return <i key={d} className={`cell cell--${level}`} title={`${d}: ${n} sets`} />; })}
        </div>
        <div className="heatkey">Less <i className="cell" /><i className="cell cell--1" /><i className="cell cell--2" /><i className="cell cell--3" /><i className="cell cell--4" /> More</div>
      </section>
    </div>
  );
}

export function History() {
  const [tab, setTab] = useState<"week" | "calendar" | "exercise">("week");
  return (
    <div>
      <div className="tabs" role="tablist" style={{ padding: "12px clamp(16px, 3vw, 28px) 0" }}>
        <button role="tab" className="btn" aria-selected={tab === "week"} onClick={() => setTab("week")}>This week</button>
        <button role="tab" className="btn" aria-selected={tab === "calendar"} onClick={() => setTab("calendar")}>Calendar</button>
        <button role="tab" className="btn" aria-selected={tab === "exercise"} onClick={() => setTab("exercise")}>Exercises</button>
      </div>
      {tab === "week" ? <WeekView /> : tab === "calendar" ? <Calendar /> : <Exercise />}
    </div>
  );
}
