import type { AppState, Snapshot } from "./snapshot";

export const base: Snapshot = {
  phase: "CODING", remainingSeconds: 359, prescription: null, progress: null,
  capacityUsed: 0, capacityLimit: 20, rotation: [], pointer: 0,
  day: { items: [{ name: "overhead", label: "Dumbbell press", kind: "lift", unit: "reps", target: 12, done: 1, total: 3 }], setsDone: 1, setsTotal: 3, complete: false },
};
export const app: AppState = {
  debug: false, setup: false, routine: { name: "Upper A", index: 1, count: 4 }, next: { name: "Lower A", index: 2, count: 4 },
  week: { done: 3, target: 4 }, lastWeight: 35, load: "per_dumbbell", lastCompleted: null, codingMinutes: 25, snoozeMinutes: 5, prompt: null, startTime: null,
};
