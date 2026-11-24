# The workout loop

1. **Coding:** the timer runs (six minutes by default).
2. **Exercise required:** the app prescribes the next set from the daily routine.
3. **Workout active:** start detection and complete the prescribed reps or time.
4. **Weight confirmation:** enter or confirm the weight when prompted.
5. **Unlocked:** return to work and continue the cycle.

Once the day's plan is complete, the timer no longer requests more sets that day.
If detection is unavailable, honor-mode completion is unverified; it is not
camera evidence. Debug mode offers manual test controls through
[`rfp debug`](../debug-mode.md).

`rfp start` begins the next set early, without waiting for the timer. Once the
day's plan is complete it reports an error instead, because nothing is owed.

Emergency release: `rfp cancel` stops the current workout without completion
credit. Cancel, snooze and skip are recorded in history as an unlock with the
reason, so every lock has a matching unlock.
See [routine configuration](../reference/config.md).
