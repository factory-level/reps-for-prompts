import { invoke } from "@tauri-apps/api/core";
import { useLayoutEffect, useRef, useState } from "react";
import type { AppState, Snapshot } from "./snapshot";
import { titleCase } from "./snapshot";

/** The backend's one answer to "is the camera/vision path working right now?" */
export type Vision = { state: "starting" | "available" | "unavailable"; reason?: string };

export const mmss = (s: number) =>
  `${String(Math.floor(s / 60)).padStart(2, "0")}:${String(Math.floor(s % 60)).padStart(2, "0")}`;

export function FullscreenButton() {
  const [on, setOn] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const toggle = () => invoke<boolean>("toggle_fullscreen").then((v) => { setOn(v); setError(null); }, (e) => setError(String(e)));
  return (
    <button className="fullscreen-toggle" onClick={() => void toggle()} title={error ?? undefined} aria-label={on ? "Exit fullscreen" : "Fullscreen"}>
      {on ? "Exit fullscreen" : "Fullscreen"}
    </button>
  );
}

/** Title sized to span the full width, capped so it never pushes the status off screen. */
export function FitTitle({ text }: { text: string }) {
  const ref = useRef<HTMLHeadingElement>(null);
  useLayoutEffect(() => {
    const el = ref.current, box = el?.parentElement;
    if (!el || !box) return;
    const fit = () => {
      el.style.fontSize = "100px";
      if (!el.scrollWidth) return;
      // Leave the siblings (status, LOGGED check) visible.
      const room = [...box.children].reduce((h, c) => (c === el || c.classList.contains("fullscreen-toggle") ? h : h - c.clientHeight), box.clientHeight);
      el.style.fontSize = `${Math.max(24, Math.min((100 * box.clientWidth * 0.96) / el.scrollWidth, (100 * room * 0.9) / el.offsetHeight))}px`;
    };
    fit();
    void document.fonts?.ready.then(fit);
    if (typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(fit);
    ro.observe(box);
    return () => ro.disconnect();
  }, [text]);
  return <h1 ref={ref} className="title">{text}</h1>;
}

/** The gym monitor's word and status for the current state (spec §4.4, M01–M08). */
function view(snapshot: Snapshot, app: AppState | null, vision: Vision | undefined, debug: boolean) {
  const rx = snapshot.prescription;
  const day = snapshot.day;
  const item = day?.items.find((i) => i.name === rx?.exercise);
  const label = item?.label ?? (rx ? titleCase(rx.exercise) : "");
  const reps = rx?.kind === "REP";
  const target = reps ? rx?.targetReps ?? 0 : rx?.targetSeconds ?? 0;
  const setOf = item ? `Set ${Math.min(item.done + 1, item.total)} of ${item.total}` : "";
  const routine = app?.routine;
  const progress = day ? `${routine?.name ?? ""} · set ${Math.min(day.setsDone + 1, day.setsTotal)} of ${day.setsTotal}` : routine?.name ?? "";
  const next = day?.items.find((i) => i.done < i.total);
  if (snapshot.paused) return { mode: "code", word: "PAUSED", lines: [<span key="a" className="small">Resume from the tray or your computer</span>] };
  switch (snapshot.phase) {
    case "IDLE":
      if (debug) return { mode: "code", word: "DEBUG", lines: [<span key="a" className="small">Idle · start a test when you’re ready</span>] };
      if (app?.setup) return {
        mode: "code", word: "READY", lines: [
          <span key="a" className="medium">{app.next?.name ?? "No routine"}</span>,
          <span key="b" className="small">Move this window to your gym screen, then start.</span>,
          <button key="c" className="gym-start" onClick={() => void invoke("start_session")}>Start session</button>,
        ],
      };
      if (app?.lastCompleted) return {
        mode: "done", word: "WORKOUT COMPLETE", lines: [
          <span key="a" className="medium">{app.lastCompleted}</span>,
          <span key="b" className="small">{app.week.done} of {app.week.target} this week · next: {app.next?.name ?? "—"}</span>,
        ],
      };
      return { mode: "code", word: "RFP", lines: [<span key="a" className="small">Start a workout from your computer</span>] };
    case "CODING":
      return {
        mode: "code", word: debug ? "DEBUG" : "CODE", lines: [
          <span key="a" className="big">{mmss(snapshot.remainingSeconds)}</span>,
          <span key="b" className="small">{next ? `Next: ${next.label} · ${next.target} ${next.unit === "reps" ? "reps" : "sec"}` : progress}</span>,
        ],
      };
    case "EXERCISE_REQUIRED":
      return {
        mode: "workout", word: "MOVE", lines: [
          <span key="a" className="medium">{label}</span>,
          <span key="b" className="small">{setOf} · {target} {reps ? "reps" : "sec"} · start from your computer</span>,
        ],
      };
    case "WEIGHT_CONFIRMATION":
      return {
        mode: "workout", word: "SET COMPLETE", lines: [
          <span key="a" className="medium">{label}</span>,
          <span key="b" className="small">Return to your computer to log it</span>,
        ],
      };
    default: {
      const manual = vision?.state === "unavailable";
      const value = Math.floor(snapshot.progress?.value ?? 0);
      return {
        mode: "workout", word: debug ? "TEST WORKOUT" : "MOVE", lines: [
          <span key="h" className="medium">{label}</span>,
          <span key="a" className="big">{reps ? `${String(value).padStart(2, "0")} / ${target}` : `${mmss(value)} / ${mmss(target)}`}</span>,
          <span key="b" className="small">{manual ? "MANUAL MODE · finish on your computer" : `${setOf} · ${reps ? "reps" : "seconds"}`}</span>,
        ],
      };
    }
  }
}

export function Screen({ snapshot, app = null, debug = false, vision }: { snapshot: Snapshot; app?: AppState | null; debug?: boolean; vision?: Vision }) {
  // Set logged → whole-screen beat before the next interval.
  if (snapshot.phase === "UNLOCKED") {
    return (
      <div className="takeover" data-phase="UNLOCKED" data-tauri-drag-region="deep">
        <FullscreenButton />
        <svg className="check" viewBox="0 0 100 100" role="img" aria-label="logged">
          <circle cx="50" cy="50" r="44" fill="var(--go)" stroke="var(--type)" strokeWidth="6" />
          <path d="M28 52 L44 68 L74 36" fill="none" stroke="var(--type)" strokeWidth="12" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
        <FitTitle text={debug ? "TEST COMPLETE" : "SET LOGGED"} />
      </div>
    );
  }
  const v = view(snapshot, app, vision, debug);
  return (
    <div className={`stage ${v.mode}`} data-phase={snapshot.phase} data-tauri-drag-region="deep">
      <FullscreenButton />
      <FitTitle text={v.word} />
      <div className="status" role="status">{v.lines}</div>
    </div>
  );
}
