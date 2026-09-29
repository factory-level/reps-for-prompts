import { invoke } from "@tauri-apps/api/core";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { Screen } from "./Screen";
import type { Snapshot } from "./snapshot";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(() => Promise.resolve()) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(() => Promise.resolve(() => {})) }));

const base: Snapshot = {
  phase: "CODING",
  remainingSeconds: 359,
  prescription: null,
  progress: null,
  capacityUsed: 0,
  capacityLimit: 20,
  rotation: [],
  pointer: 0,
  day: {
    items: [{ name: "jumprope", label: "Jump rope", kind: "jumprope", unit: "seconds", target: 60, done: 0, total: 2 }],
    setsDone: 7,
    setsTotal: 31,
    complete: false,
  },
};

describe("Screen", () => {
  it("CODING shows CODE and the countdown", () => {
    render(<Screen snapshot={base} variant="primary" />);
    expect(screen.getByText("CODE")).toBeInTheDocument();
    expect(screen.getByText(/05:59/)).toBeInTheDocument();
  });

  it("primary offers a gentle reminder before starting the camera", () => {
    render(<Screen snapshot={{ ...base, phase: "EXERCISE_REQUIRED" }} variant="primary" />);
    expect(screen.getByText("WORKOUT")).toBeInTheDocument();
    expect(screen.queryByRole("img", { name: "locked" })).not.toBeInTheDocument();
    expect(screen.getByText(/start when ready/)).toBeInTheDocument();
  });

  it("gym shows the live count in the prescription's unit", () => {
    const snap: Snapshot = {
      ...base,
      phase: "WORKOUT_ACTIVE",
      prescription: { exercise: "jumprope", kind: "CONTINUOUS", targetReps: 0, targetSeconds: 60, defaultWeight: 0 },
      progress: { value: 42.7, unit: "seconds", satisfied: false },
    };
    render(<Screen snapshot={snap} variant="gym" />);
    expect(screen.getByText("Jump rope")).toBeInTheDocument();
    expect(screen.getByText("042")).toBeInTheDocument();
    expect(screen.getByText("/ 60 sec")).toBeInTheDocument();
  });
});

it("Debug stays idle without displaying an automatic countdown", () => {
  render(<Screen snapshot={base} variant="primary" debug />);
  expect(screen.getByText("DEBUG")).toBeInTheDocument();
  expect(screen.getByText(/Idle · start a test/)).toBeInTheDocument();
  expect(screen.queryByText(/05:59/)).not.toBeInTheDocument();
});
it("Debug test progress never claims the screen is locked", () => {
  render(<Screen snapshot={{ ...base, phase: "WORKOUT_ACTIVE" }} variant="primary" debug />);
  expect(screen.getByText("TEST WORKOUT")).toBeInTheDocument();
  expect(screen.queryByRole("img", { name: "locked" })).not.toBeInTheDocument();
  expect(screen.getByText(/not saved to your workouts/)).toBeInTheDocument();
});

beforeEach(()=>{vi.mocked(invoke).mockReset();});
it("starts through the shared CLI action and prevents duplicate submissions",async()=>{
 let finish!:()=>void;
 vi.mocked(invoke).mockImplementation(()=>new Promise<void>(resolve=>{finish=resolve;}));
 render(<Screen snapshot={base} variant="primary"/>);
 const button=screen.getByRole('button',{name:'Start workout'});
 fireEvent.click(button);fireEvent.click(button);
 expect(invoke).toHaveBeenCalledTimes(1);
 expect(invoke).toHaveBeenCalledWith('workout_action',{action:'start',weight:null});
 expect(button).toBeDisabled();finish();
 await waitFor(()=>expect(button).toBeEnabled());
});
it("logs fractional weight using the shared finish action",async()=>{
 vi.mocked(invoke).mockResolvedValue({});
 render(<Screen snapshot={{...base,phase:'WEIGHT_CONFIRMATION',prescription:{exercise:'squat',kind:'REP',targetReps:5,targetSeconds:0,defaultWeight:45}}} variant="primary"/>);
 fireEvent.change(screen.getByLabelText('Weight (lb)'),{target:{value:'47.5'}});
 fireEvent.click(screen.getByRole('button',{name:'Log weight'}));
 await waitFor(()=>expect(invoke).toHaveBeenCalledWith('workout_action',{action:'finish',weight:47.5}));
});
it("shows action failures so a failed save is not presented as logged",async()=>{
 vi.mocked(invoke).mockRejectedValue('Set is not complete');
 render(<Screen snapshot={{...base,phase:'WEIGHT_CONFIRMATION'}} variant="primary"/>);
 fireEvent.click(screen.getByRole('button',{name:'Log weight'}));
 expect(await screen.findByRole('alert')).toHaveTextContent('Set is not complete');
});
it("keeps gym screens and completed routines free of start controls",()=>{
 const {rerender}=render(<Screen snapshot={base} variant="gym"/>);
 expect(screen.queryByRole('button')).not.toBeInTheDocument();
 rerender(<Screen snapshot={{...base,day:{...base.day!,complete:true}}} variant="primary"/>);
 expect(screen.queryByRole('button')).not.toBeInTheDocument();
});
it("shows whether the detector sees a pose during an active set",async()=>{
 const {listen}=await import("@tauri-apps/api/event");
 const handlers:Record<string,(e:{payload:unknown})=>void>={};
 vi.mocked(listen).mockImplementation(((name:string,fn:(e:{payload:unknown})=>void)=>{handlers[name]=fn;return Promise.resolve(()=>{});}) as never);
 const rx={exercise:"squat",kind:"REP" as const,targetReps:5,targetSeconds:0,defaultWeight:0};
 render(<Screen snapshot={{...base,phase:"WORKOUT_ACTIVE",prescription:rx}} variant="primary"/>);
 await waitFor(()=>expect(handlers["vision-pose"]).toBeDefined());
 handlers["vision-pose"]({payload:{detected:false}});
 expect(await screen.findByText(/Not in frame/)).toBeInTheDocument();
 handlers["vision-pose"]({payload:{detected:true}});
 expect(await screen.findByText(/In frame · tracking/)).toBeInTheDocument();
});
