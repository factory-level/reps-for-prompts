export type Phase =
  | "IDLE"
  | "CODING"
  | "EXERCISE_REQUIRED"
  | "WORKOUT_ACTIVE"
  | "WEIGHT_CONFIRMATION"
  | "UNLOCKED";

export interface Prescription {
  exercise: string;
  kind: "REP" | "CONTINUOUS";
  targetReps: number;
  targetSeconds: number;
  defaultWeight: number;
}

export interface Progress {
  value: number;
  unit: string;
  satisfied: boolean;
}

// One line of the day's routine (a lift, jump-rope, or a stretch) with how many
// of its sets/rounds are done. "One set per lock" decrements `done` each time.
export interface DayItem {
  name: string;
  label: string;
  kind: "lift" | "jumprope" | "stretch";
  unit: "reps" | "seconds";
  target: number; // reps per set, or seconds per round/hold
  done: number; // sets/rounds/holds completed
  total: number; // sets/rounds/holds required today
}

export interface DayPlan {
  items: DayItem[];
  setsDone: number; // completed across all items
  setsTotal: number; // required across all items
  complete: boolean; // every item done → day cleared
}

export interface Snapshot {
  phase: Phase;
  remainingSeconds: number;
  prescription: Prescription | null;
  progress: Progress | null;
  capacityUsed: number;
  capacityLimit: number;
  rotation: string[];
  pointer: number;
  // Present once the routine.json plan is wired; optional so the UI degrades
  // gracefully before then.
  day?: DayPlan | null;
  paused?: boolean;
}

export type PromptKind = "start" | "due" | "exercising" | "confirm";

/** Everything the windows show besides the snapshot (Rust `ui::state`). */
export interface AppState {
  debug: boolean;
  setup: boolean;
  routine: { name: string; index: number; count: number } | null;
  next: { name: string; index: number; count: number } | null;
  week: { done: number; target: number };
  lastWeight: number | null;
  load: "total" | "per_dumbbell" | "none";
  lastCompleted: string | null;
  codingMinutes: number;
  snoozeMinutes: number;
  prompt: PromptKind | null;
  startTime: string | null;
}

export interface PlanItem {
  exercise: string;
  label: string;
  kind: "lift" | "timed";
  sets: number;
  reps: number;
  seconds: number;
  defaultWeight: number | null;
  load: "total" | "per_dumbbell" | "none";
}
export interface Routine { id: string; name: string; archived: boolean; items: PlanItem[] }
export interface TrainingPlan {
  routines: Routine[];
  cycle: { slotId: string; routineId: string }[];
  nextSlot: string;
  weekdays: number[];
  weeklyTarget: number;
  startTimes: string[];
  codingMinutes: number;
  snoozeMinutes: number;
}

export interface SetRow {
  id: number; date: string; exercise: string; kind: "rep" | "continuous";
  reps: number; seconds: number; weight: number; verified: boolean; workoutId: string | null;
}
export interface WorkoutRow { id: string; routine: string; status: "active" | "completed" | "partial" | "canceled"; date: string; sets: number }

/** Exercise ids are detector names (squat, incline_bench): show them as words. */
export const titleCase = (id: string) => id.replace(/[_-]+/g, " ").replace(/\b\w/g, (c) => c.toUpperCase());
