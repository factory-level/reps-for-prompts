import { invoke } from "@tauri-apps/api/core";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { Prompt } from "./Prompt";
import { app, base } from "./fixtures";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(() => Promise.resolve()) }));
const press = { exercise: "overhead", kind: "REP", targetReps: 12, targetSeconds: 0, defaultWeight: 0 } as const;
const confirm = { ...base, phase: "WEIGHT_CONFIRMATION" as const, prescription: press, progress: { value: 11, unit: "reps", satisfied: true } };
beforeEach(() => vi.mocked(invoke).mockClear());

describe("work desktop prompt", () => {
  it("P02 due: start, snooze, pause", () => {
    render(<Prompt snapshot={{ ...base, phase: "EXERCISE_REQUIRED", prescription: press }} app={{ ...app, prompt: "due" }} />);
    expect(screen.getByText("Dumbbell press")).toBeInTheDocument();
    expect(screen.getByText("Set 2 of 3 · 12 reps")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Start set" }));
    expect(invoke).toHaveBeenCalledWith("start_set", undefined);
    expect(screen.getByRole("button", { name: "Snooze 5m" })).toBeInTheDocument();
  });
  it("Escape and × are Not now", () => {
    render(<Prompt snapshot={{ ...base, phase: "EXERCISE_REQUIRED", prescription: press }} app={{ ...app, prompt: "due" }} />);
    fireEvent.keyDown(window, { key: "Escape" });
    fireEvent.click(screen.getByRole("button", { name: "Not now" }));
    expect(vi.mocked(invoke).mock.calls.filter(([n]) => n === "dismiss_prompt")).toHaveLength(2);
  });
  it("P03 prefills detected reps and the last weight, labels the load, Enter saves once", async () => {
    render(<Prompt snapshot={confirm} app={{ ...app, prompt: "confirm" }} />);
    expect(screen.getByLabelText("Reps")).toHaveValue("11");
    const weight = screen.getByLabelText("Weight");
    expect(weight).toHaveValue("35");
    expect(weight).toHaveFocus();
    expect(screen.getByText("lbs / dumbbell")).toBeInTheDocument();
    fireEvent.change(weight, { target: { value: "35.5" } });
    fireEvent.submit(weight.closest("form")!);
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("confirm_set", { weight: 35.5, reps: 11 }));
  });
  it("P03 rejects a bad weight and keeps the draft", () => {
    render(<Prompt snapshot={confirm} app={{ ...app, prompt: "confirm", lastWeight: null }} />);
    const weight = screen.getByLabelText("Weight");
    expect(weight).toHaveValue("");
    fireEvent.change(weight, { target: { value: "-3" } });
    fireEvent.submit(weight.closest("form")!);
    expect(screen.getByRole("alert")).toHaveTextContent("0 or more");
    expect(weight).toHaveValue("-3");
    expect(invoke).not.toHaveBeenCalledWith("confirm_set", expect.anything());
  });
  it("a failed save keeps the inputs and says why", async () => {
    vi.mocked(invoke).mockImplementationOnce(() => Promise.reject("This set was already saved"));
    render(<Prompt snapshot={confirm} app={{ ...app, prompt: "confirm" }} />);
    fireEvent.submit(screen.getByLabelText("Weight").closest("form")!);
    expect(await screen.findByRole("alert")).toHaveTextContent("already saved");
    expect(screen.getByLabelText("Weight")).toHaveValue("35");
  });
  it("the last set saves and finishes", () => {
    render(<Prompt snapshot={{ ...confirm, day: { ...confirm.day!, setsDone: 2 } }} app={{ ...app, prompt: "confirm" }} />);
    expect(screen.getByRole("button", { name: "Save & finish workout" })).toBeInTheDocument();
  });
  it("P01 start reminder shows the week and the next routine", () => {
    render(<Prompt snapshot={{ ...base, phase: "IDLE", day: null }} app={{ ...app, prompt: "start", routine: null }} />);
    expect(screen.getByText("Next: Lower A")).toBeInTheDocument();
    expect(screen.getByText("3 of 4 workouts this week")).toBeInTheDocument();
  });
  it("exercising offers manual completion", () => {
    render(<Prompt snapshot={{ ...base, phase: "WORKOUT_ACTIVE", prescription: press }} app={{ ...app, prompt: "exercising" }} vision={{ state: "unavailable" }} />);
    fireEvent.click(screen.getByRole("button", { name: "I finished this set" }));
    expect(invoke).toHaveBeenCalledWith("manual_finish", undefined);
  });
});
