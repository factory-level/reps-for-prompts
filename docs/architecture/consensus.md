# RFP consensus implementation

The desktop saves camera choices in `cameras.json` under normal application data.
Timing calibration is loaded from sibling `camera-calibration.json` at enable.
Optional `REPS_PHONE_URL_FILE` supplies credentials without putting them in the
form. Camera registration failures propagate; they cannot silently enable the
wrong camera set. Existing USB rotation is preserved.

The Rust client negotiates preview-frame subscription with API 1.6 while keeping
older subscription transcripts. ProgressContext carries consensus diagnostics;
the engine remains hub-free. The desktop displays hub counts, diagnostic states,
and bounded-rate camera previews. Settings are disabled during a workout.

MovementActivity reports completed ordered phase sequences and minimum measured
joint visibility. StreamLoop adds stream epoch and calibrated timing metadata.
LiveCapture owns one reader per device, drains into a latest-frame slot and
reconnects with capped backoff. Reconnect and long gaps invalidate alignment.
Missing calibration yields no eligible evidence. Timing is a measured arrival
clock estimate, not native synchronized camera clocks. The actual phone and
held-out accuracy remain qualification gates; see the hub architecture report.
