# E2E checklist — target machine

Run from the repo root with the hub checked out as a sibling (or `HUB_DIR` set).
Everything in the automated section is hardware-free except the two camera items,
and none of it writes real workout history.

## Automated — software evidence

- [ ] `node scripts/qualify-software.mjs --output DIR --with-bundle --with-video`
      → `summary.md` says PASS. Covers hub typecheck/vitest/pytest, the reps
      detector, desktop vitest + `tsc --noEmit` + `cargo test --workspace`,
      the movement benchmark and the bundled e2e. Note any recorded
      public-video challenge failures.

## Automated — pipeline (fixture video, no webcam)

- [ ] `node scripts/e2e-latency.mjs --movement-contract` → `E2E PASS`,
      p95 < 50 ms, reps == 2, pinned `movementVersion` + model sha256
- [ ] `./scripts/bundle-hub.sh` then
      `node scripts/e2e-latency.mjs --bundle --parent-exit` → same result
      through the staged bundle, plus `parent-pipe shutdown PASS`
- [ ] `node scripts/e2e-two-camera.mjs` → `E2E TWO-CAMERA PASS`; best-view
      election fails over and the rep count survives the occlusion

## Automated — hub gates (fixture video, no webcam)

- [ ] `usb-mcp-hub/scripts/demo-n1.sh` → `DEMO N1 COMPLETE`; the action-result
      query shows `snapshot.executed` / `succeeded` with an `imageRef`
- [ ] `usb-mcp-hub/scripts/demo-n2.sh` → `DEMO N2 COMPLETE`; propose without
      consent is refused, the webhook receives `prompt_matched`

Both honor `PORT` / `DEBUG_PORT` if their defaults are taken.

## Automated — installed build (needs `install-dogfood.py` and a webcam)

Requires `rfp.service` running and idle (`rfp status` → phase `CODING`, with a
few minutes left on the timer). `rfp cancel` is the no-credit escape if the
workout screen appears mid-run.

- [ ] `python3 scripts/verify-cli-dogfood.py` → both `PASS:` lines. Opens the
      real webcam for preview; asserts real history is unchanged and restores
      workout mode.
- [ ] `usb-mcp-hub/scripts/verify-camera-gating.sh 8081` → `CAMERA GATING OK`
      (the installed hub's debug listener is local and unauthenticated)
- [ ] `python3 scripts/verify-passive.py` → snooze survives restart, zero credit
- [ ] `python3 scripts/verify-daemon.py` → run **last**; it stops and restarts
      `rfp.service`
- [ ] Read-only surfaces: `rfp inspect`, `rfp site`, `rfp profile`,
      `rfp status --source remote`, `rfp summary`

Afterwards confirm nothing moved: `rfp history` matches the pre-run capture,
`rfp status` shows `mode: workout`, and `systemctl --user show rfp.service -p
NRestarts` is unchanged.

## Manual — real webcam and phone

- [ ] Timer expires → **Start workout** → live skeleton, knee angle updates,
      squats count into the session, reaching weight confirmation without
      `simulate_progress`
- [ ] Kill the hub mid-set (`pkill -f hubd.mjs`): one restart happens, the set
      continues
- [ ] Kill it again: **Done (honor)** completes the set; the `exercise_history`
      row has `verified = 0`
- [ ] Camera LED: on only between starting the workout and unlock — never
      during coding
- [ ] Phone tuning app (`https://<lan-ip>:8443/`, pairing token required):
      enable `reps_vision`, live angle readout, drag a threshold slider
      mid-preview, capture snapshots + description, Save draft
- [ ] Jump rope: prescription streams motion, duration accrues, a 2 s pause
      resets the streak
