import { invoke } from "@tauri-apps/api/core";
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { Screen } from "./Screen";
import { app, base } from "./fixtures";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(() => Promise.resolve(true)) }));

const press = { exercise: "overhead", kind: "REP", targetReps: 12, targetSeconds: 0, defaultWeight: 0 } as const;

describe("gym monitor", () => {
  it("coding shows CODE, the countdown and the next exercise", () => {
    render(<Screen snapshot={base} app={app} />);
    expect(screen.getByText("CODE")).toBeInTheDocument();
    expect(screen.getByText("05:59")).toBeInTheDocument();
    expect(screen.getByText(/Next: Dumbbell press · 12 reps/)).toBeInTheDocument();
  });
  it("a live set shows done / target as the biggest number", () => {
    render(<Screen snapshot={{ ...base, phase: "WORKOUT_ACTIVE", prescription: press, progress: { value: 8.4, unit: "reps", satisfied: false } }} app={app} />);
    expect(screen.getByText("08 / 12")).toBeInTheDocument();
    expect(screen.getByText("DUMBBELL PRESS")).toBeInTheDocument();
  });
  it("timed sets count in mm:ss", () => {
    render(<Screen snapshot={{ ...base, phase: "WORKOUT_ACTIVE", prescription: { ...press, kind: "CONTINUOUS", targetReps: 0, targetSeconds: 60 }, progress: { value: 32, unit: "seconds", satisfied: false } }} app={app} />);
    expect(screen.getByText("00:32 / 01:00")).toBeInTheDocument();
  });
  it("without the camera it says manual mode and invents no progress", () => {
    render(<Screen snapshot={{ ...base, phase: "WORKOUT_ACTIVE", prescription: press }} app={app} vision={{ state: "unavailable", reason: "hub not running" }} />);
    expect(screen.getByText(/MANUAL MODE/)).toBeInTheDocument();
    expect(screen.getByText("00 / 12")).toBeInTheDocument();
  });
  it("weight entry happens on the computer, not the monitor", () => {
    const { container } = render(<Screen snapshot={{ ...base, phase: "WEIGHT_CONFIRMATION", prescription: press }} app={app} />);
    expect(screen.getByText("SET COMPLETE")).toBeInTheDocument();
    expect(screen.getByText("Return to your computer to log it")).toBeInTheDocument();
    expect(container.querySelector("input")).toBeNull();
  });
  it("setup offers Start session; completion shows the week", () => {
    const { rerender } = render(<Screen snapshot={{ ...base, phase: "IDLE", day: null }} app={{ ...app, setup: true }} />);
    expect(screen.getByText("READY")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Start session" }));
    expect(invoke).toHaveBeenCalledWith("start_session");
    rerender(<Screen snapshot={{ ...base, phase: "IDLE", day: null }} app={{ ...app, lastCompleted: "Upper A" }} />);
    expect(screen.getByText("WORKOUT COMPLETE")).toBeInTheDocument();
    expect(screen.getByText(/3 of 4 this week · next: Lower A/)).toBeInTheDocument();
  });
  it("paused and logged states", () => {
    const { rerender } = render(<Screen snapshot={{ ...base, paused: true }} app={app} />);
    expect(screen.getByText("PAUSED")).toBeInTheDocument();
    rerender(<Screen snapshot={{ ...base, phase: "UNLOCKED" }} app={app} />);
    expect(screen.getByText("SET LOGGED")).toBeInTheDocument();
  });
  it("offers a fullscreen toggle on every screen", () => {
    render(<Screen snapshot={base} app={app} />);
    fireEvent.click(screen.getByRole("button", { name: "Fullscreen" }));
    expect(invoke).toHaveBeenCalledWith("toggle_fullscreen");
  });
});
