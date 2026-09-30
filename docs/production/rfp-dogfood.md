# RFP: reps for prompts — dogfooding

RFP runs in the Linux desktop session while Codex or Claude is open. It is a
local process-presence detector, not a reader of prompts or transcripts.
Native Codex, native Claude (including version-named executables in
`~/.local/share/claude/versions/`), and their standard Node CLI entrypoints are
recognized. Detection is scoped to the current Unix user and refreshed every
five seconds. Browser tabs and remote SSH processes on other machines are not
detected. Open idle agents count as open by design.

## Use it

The tray offers **Open Reps**, **Snooze 15 minutes**, and **Quit Reps**. Closing
the main window in Workout mode hides it. Quit exits until the next login or
manual service start. A crash restarts it after five seconds.

Gentle reminders default to 25 minutes of agent-open time. Start the workout
when ready; the reminder itself does not start camera capture or lock other
windows. Snooze 5/15/30 minutes or skip a break in the app. Snooze persists
across restarts, and reminders wait for an agent if snooze expires while both
are closed. Sleep/scheduling gaps over five seconds are excluded. The counter
restarts at the configured interval after a completed or skipped break.

Settings expose interval, optional lock mode, and an editor for existing routine
movements. Debug is still isolated and never uploads simulated workouts.
The emergency escape remains Ctrl+Shift+Backspace held three seconds.

```sh
reps status --json
reps history --json --limit 50
reps summary --json --from 2026-09-01 --to 2026-09-30
systemctl --user status rfp.service
journalctl --user -u rfp.service -n 30
```

Local history commands are read-only and don't launch the camera or desktop.
Use `--cursor` with `nextCursor` to page history. Legacy rows keep their original
dates; missing exact timestamps and weight units are `null`. New weights are
explicitly pounds, matching the existing weight-entry screen. The local source
contains this machine's history, and remote contains the configured dataset.

## Install or update on Linux

```sh
pnpm --dir app build
(cd app/src-tauri && cargo build --release --workspace --features custom-protocol)
python3 scripts/install-dogfood.py
```

Resources must first have been staged with `scripts/bundle-hub.sh`. The installer
copies binaries/resources under `~/.local/lib/rfp`, links `~/.local/bin/reps`,
and creates `~/.config/autostart/rfp.desktop` plus a user systemd service.
It switches to gentle Workout mode and a 25-minute interval, preserving a
SQLite backup and the previous mode. It never deletes workout history.
Re-running installation resets that interval and mode intentionally.

To stop startup: remove `~/.config/autostart/rfp.desktop` and run
`systemctl --user stop rfp.service`. Keep the data directory to retain history.

## Private sync and public posting

The web backend must be configured before uploads can start. See
[the web setup guide](../../web/README.md). In the normal app data directory,
create `upload.json` for desktop sync and `remote.json` for agent reads:

```json
{"url":"https://reps-for-prompts.vercel.app","token":"YOUR_SCOPED_TOKEN"}
```

Give both files mode `0600`. Credentials are never put in command arguments.
The desktop syncs every five minutes outside its UI/state-machine thread.
Failures back off exponentially to one hour.
SQLite's `synced` flag is the durable upload queue; an acknowledgement marks
only accepted IDs. Lost responses are safe to retry. A successful empty sync
also drains eligible server-side public posts after a rate limit expires.

```sh
reps sync --json
reps history --source remote --json
reps summary --source remote --json
reps status --source remote --json
```

An operator explicitly enables automatic public posting for the private dataset.
Only workouts with an exact timestamp on or after this opt-in are eligible;
legacy history is synced privately. Public posts contain nickname, exercise,
reps/duration, and a generic note, never weights, credentials, or camera media.
Camera verification is not treated as independent public proof.

Limits:

- Automatic posts: six per rolling hour per dataset, with excess queued.
- Upload API: 12 requests per minute per credential.
- Read API: 120 requests per minute per credential.
- Anonymous web posts: ten per fixed hour per IP hash.
- Email signup: five per fixed hour per IP hash.
- Cheers/reports: 100 requests per fixed hour per IP hash, deduplicated per post.

Anonymous nicknames aren't identities. Rate limiting reduces casual abuse;
people sharing an IP share limits and a cheer. Moderation remains necessary.

## Verification and remaining launch checks

Automated checks cover process matching, pause/sleep timer behavior, instance
locking, migration, readonly access, retry deduplication, dataset isolation,
credential revocation, posting rate limits, and the actual CLI over HTTP to a
local Postgres-backed site. Browser checks cover mobile submission, deletion,
error handling, and horizontal overflow.

Physical-camera accuracy, the emergency chord on the installed app, and a full
workday soak still need hands-on qualification. Existing fixture checks are not
a substitute for that trial. A missing remote configuration is shown as
**Not configured**; no public posting occurs until the database and credentials
are connected.

### Local verification — 2026-09-24

- Installed user service was running in Cinnamon/X11 with both actual Codex and
  version-named native Claude processes detected.
- Hub health: vision host up, camera closed, zero enabled metrics while coding.
- Second desktop launch rejected by the instance lock.
- Controlled idle-service crash recovered automatically (`NRestarts=1`).
- `scripts/verify-passive.py` passed two isolated installed-app launches: snooze
  preserved its deadline, timer stayed paused, zero workout credit was created.
- Rust workspace: 78 tests passed, one existing fixture supervisor test ignored.
- Desktop frontend: 21 tests passed. Web production build passed. Postgres-backed
  API tests and the actual CLI → HTTP → public post → remote read test passed.
- Mobile browser submission/deletion passed with no horizontal overflow.
- Site deployed at https://reps-for-prompts.vercel.app. Production database and
  upload/read credentials remain unconfigured; posting is visibly disabled.


### Cloud setup and usage guards — 2026-09-25

- Supabase project `jfzlaafxlquwkluohpuh` created for RFP in `us-east-1`.
- Owner confirmed Supabase Free and Vercel Hobby. No paid upgrades enabled.
- Database schema, shared request budgets, storage caps, and the approved
  restricted `rfp_web` login applied. Runtime connection verified; advisors clean.
- Four Postgres API tests pass, including concurrent budget exhaustion, window
  recovery, concurrent storage caps, idempotency at capacity, and runtime grants.
- Rebuilt desktop installed and running; five-minute sync with exponential
  failure backoff (up to an hour), both Codex and Claude detected.
- Production code deployed, but cloud posting remains disconnected pending
  Vercel CLI sign-in and persistent production environment variables. The
  deployment connector accepted its request but did not apply the env fields;
  `/api/activity` still reported `configured: false` at this checkpoint.
- Prepared credential files are private to this computer. Desktop upload/read
  config will only be installed after the production connection is verified.

See `web/README.md` for exact limits and the distinction between application
quotas and provider billing controls. No application rate limiter can guarantee
free quota availability during arbitrary external traffic.


### Live connection verified — 2026-09-25

- Vercel CLI authenticated as `admin-60506784`; RexHome is confirmed Hobby by
  the CLI. Its complete project listing contains only `reps-for-prompts`; no
  other projects were found to pause and no projects were deleted.
- Approved production `DATABASE_URL` and `RATE_LIMIT_SECRET` are stored as
  sensitive Vercel project environment variables. Redeployment
  `dpl_2Fgz1zrqWGkR4HypBHD7pe6zJP7Z` is ready at
  https://reps-for-prompts.vercel.app; `/api/activity` reports configured=true.
- Mode-0600 upload/read configs installed on this computer. Real CLI upload
  acknowledged two historical records; remote summary returned two sets and
  ten reps. Historical records remain private; the public feed is empty until
  a new eligible workout or community post is submitted.
- Automatic publishing is enabled for the owner's dataset as “RFP founder”
  from its provisioning timestamp, limited to six posts per rolling hour.
- Desktop remains running with both agents detected. No credentials are in
  this repository or browser code. The earlier disconnected checkpoints above
  are superseded by this successful connection.

## Local CLI controls

`rfp` is the local interface (`reps` remains an alias). CODE/WORKOUT screens
stay available as displays; no tray, website form, or weight-entry click is
required for the workout flow.

```sh
rfp service start
rfp status --json                    # includes the live prescription/progress
rfp start                           # begins next prescribed workout; camera on
rfp finish --weight 0                # after the camera detects the target
rfp finish --honor --weight 0        # only if you personally completed the set
rfp cancel                          # abort; no completion credit
rfp snooze --minutes 15
rfp skip
rfp settings --minutes 25 --lock-mode off
rfp routine                         # display current routine JSON
rfp routine --file ./routine.json    # update between workouts
rfp show
rfp hide
rfp history --json
rfp summary --source remote --json
rfp sync                            # explicit upload; automatic upload stays on
rfp service stop
```

Finish weights are pounds, default zero. A set that needs weight confirmation
is only logged by `finish`; honor completions are marked unverified. Continuous
workouts can finish automatically when their duration is detected. Invalid or
repeated finish commands cannot create completion credit. Controls return live
session state and accept `--json`; status/history remain readable with the
service stopped. Full GUI removal is not required: screens may still show
optional controls, but the local CLI supports the daily interaction flow.

Commands use a mode-0600 Unix socket in the existing data directory. On Linux,
the server additionally checks the peer UID. Messages and timeouts are bounded,
and the socket has no network listener or cloud credential. The app owns all
session mutations; the CLI doesn't patch the database behind the running timer.
The existing five-minute upload/backoff and server usage guards are unchanged.

CLI installation verification: 25 app/control unit tests passed, release build
passed, and installed commands verified live status, settings, routine reads,
snooze/skip, and service status. CLI start showed an active workout; repeat
start was rejected; cancel returned to CODE. Finish without an active workout
and invalid settings were rejected. The two existing history rows were
unchanged throughout; no test workout was credited or published.

## CLI-only dogfooding (supersedes optional GUI controls above)

The app now renders only CODE/WORKOUT, camera, or fixture-video displays.
There are no application buttons, forms, tray menus, keyboard action shortcuts,
or weight inputs. The title bars are hidden too. CLI display commands select a
monitor and fullscreen state. Lock takeover stays off so the terminal remains
accessible; `rfp cancel` is the workout escape path.

```sh
rfp mode debug                     # isolated, temporary history; no uploads
rfp debug exercises
rfp debug start --exercise squat
rfp debug step                     # or --value 3
rfp debug done
rfp finish --weight 0               # debug credit remains temporary
rfp debug history
rfp debug stop
rfp debug videos
rfp debug video --exercise squat --file /path/to/fixture.webm
rfp debug video-stop

rfp camera list
rfp camera settings
rfp camera set --device /dev/video0 --rotation 180
rfp camera set --file ./cameras.json # same JSON fields shown by settings
rfp camera preview                  # idle preview does not create workout credit
rfp camera status                   # health, preview frames, errors
rfp camera stop                     # stops idle preview; active workout camera stays on

rfp display --window main           # list monitor indexes
rfp display --window gym --monitor 1 --fullscreen on --visible on
rfp display --window gym --visible off
rfp mode workout                    # restart into real recording + queued uploads
rfp routine > routine.json
# Edit routine.json in a terminal editor.
rfp routine --file ./routine.json
rfp settings --minutes 25
rfp start
rfp finish --weight 0
rfp service logs
```

Mode switching saves the selected mode, then the CLI restarts the user service
and waits for a matching live response. Cancel or finish an active session first.
Debug routine edits apply only to the temporary session; switch to Workout to
save the real routine. Camera settings apply to the installation and can only
change while idle with preview stopped. `camera stop` returns to CODE/WORKOUT;
use `cancel` to stop a live workout and release its camera.

Camera preview pauses the coding timer and discards all counting events.
Frames are forwarded only to the camera display, never uploaded or logged.
Simulation and fixture inspection require Debug mode. Normal `history` still
reads real history; `debug history` explicitly reads the temporary test store.

CLI-only verification completed on the installed computer: 25 frontend tests
passed (including no interactive elements across all four display states),
61 app/engine/CLI tests passed, and release builds passed. The installed CLI
script verified both mode switches, simulated debug completion into temporary
history, routine roundtrip, bundled fixture frames, live camera preview frames,
input rejection, and display control. Real history remained at two records;
no debug records were uploaded. The final 1280×720 CODE window was visually
checked without controls; the camera was closed and no metrics remained enabled.

## Passive daemon inspection and end-of-day warning

The installed `rfp.service` starts at desktop login through XDG autostart and restarts on failure. It stays running with its windows hidden. Every five seconds it checks same-user Codex/Claude Code process presence; it does not read prompts or track foreground usage. Computer sleep and absence of either agent pause the work timer.

```sh
rfp inspect                 # daemon PID, latest process sample, countdown and pause reason
rfp inspect --watch         # refresh every two seconds; Ctrl-C exits inspection only
rfp inspect --watch --json  # newline-delimited JSON for local agents
rfp workday --end 18:00 --warn-minutes 60
rfp service logs
```

When a break is due, the daemon shows the WORKOUT display and sends a desktop notification asking you to run `rfp start`. The camera remains off until that command. From 5 PM local time (6 PM workday end minus 60 minutes), it also warns if routine sets remain. A late login catches up the warning. The warning respects snooze, camera preview, Debug mode and active workouts; it waits for a coding agent to be open. It is recorded in SQLite at most once per local date across restarts and shown in the passive display and inspection output. It never marks a workout complete or posts a workout by itself. Notification delivery may be hidden by desktop Do Not Disturb; the display and daemon log remain available.

## Minimal on-screen workout controls

The primary CODE/WORKOUT screen also offers **Start workout** (equivalent to `rfp start`) and, when a set is complete, a **Weight (lb)** field with **Log weight** (equivalent to `rfp finish --weight N`). Both use the CLI's validated action handlers. Gym displays remain passive. Routines, modes, camera previews, snooze, cancellation, and service management stay in the CLI.

## Profile, automatic sharing, and selectable sites

Completed real workouts upload automatically every five minutes, with failure backoff up to an hour. Public sharing is on by default when configuring a new destination. Debug results stay isolated. A profile is a nickname plus sharing preferences for an upload credential, not a website account.

```sh
rfp site                                      # active endpoint, credential presence; no secrets
rfp profile                                   # nickname, sharing, last sync, public boundary
rfp profile --nickname "Your nickname"
rfp profile --sharing off                     # private sync continues; no public posts
rfp profile --sharing on                      # share future results; no private-period backfill
rfp site --url http://localhost:3000 --upload-token-file /path/local-upload.token --read-token-file /path/local-read.token
rfp site --url https://reps-for-prompts.vercel.app   # restores saved live credentials
rfp sync                                      # explicit immediate retry, subject to server limits
```

Token files must contain the raw issued token and be mode 0600. New endpoints require their own upload credential; existing credentials are never copied to a new host. Site origins allow HTTPS, or HTTP only on loopback. Credentials are kept in mode-0600 `sites.json`; legacy upload/read configs are imported on first configuration. Reads, manual sync and the daemon all use the selected endpoint. The daemon notices changes on its next scheduled upload without a restart. Acknowledgments are scoped to endpoint and credential, so testing locally cannot consume the production upload queue. Switching to an existing site preserves its sharing setting. Switching to a new site enables sharing from that moment, while older records may be synced privately. Existing installations retain their current server profile and sharing settings.

Use a separate local database and locally issued dataset tokens when testing. A localhost web server connected to the production database is still production data. Apply `20260925215359_rfp_profile_settings.sql` after the runtime-role migration to allow the upload-authenticated profile API to update its three preference columns. Public posting remains limited to six sets per dataset per hour, with queued posts drained on later syncs.

## Location and relative routine progress

```sh
rfp profile --location "Oakland, CA"  # explicitly publish this label
rfp profile --clear-location         # remove it from profile and all past posts
```

Location is unset by default and never detected automatically. Each site has its own profile. Completed/target daily routine snapshots sync with workouts; the showcase colors each day by that guest's own completion percentage. Tracking begins when this version runs, with no invented historical goals. Missing days remain untracked. The site is now a short GitHub-linked explanation beside rotating guest heatmaps; labeled demo profiles fill empty spaces without database writes.

### CLI end-to-end dogfood — 2026-09-25

Whole ladder run in one sitting against the **installed** build
(`~/.local/lib/rfp`, hub API 1.6), reps `b03d973` + hub `bd0d726`. Zero real
workout credit: `rfp history` was byte-identical before and after, the day
stayed at 7/31 sets, `NRestarts=0`, and no coredumps.

- `qualify-software.mjs --with-bundle --with-video`: PASS, 9/9 checks. Hub 231
  tests / 49 files, hub pytest 118, detector pytest 137, desktop 30 tests / 6
  files, `tsc --noEmit` clean, Rust 90 passed / 1 ignored. Recorded
  public-video challenge failure: `curl-camera-cuts`.
- `e2e-latency.mjs --movement-contract`: E2E PASS, frames=198, reps=2,
  p50 22.7 ms / p95 26.8 ms.
- `e2e-latency.mjs --bundle --parent-exit`: E2E PASS, frames=192, reps=2,
  p50 22.8 ms / p95 27.0 ms, parent-pipe shutdown clean.
- `e2e-two-camera.mjs`: PASS, fusedLandmarks=219, reps=2, election
  `null->front,front->side` — count survived the occlusion.
- Hub `demo-n1.sh`: PASS. Snapshot action now verified through
  `query_history action_result`: `snapshot.executed` succeeded in 18 ms with a
  real `imageRef`.
- Hub `demo-n2.sh`: PASS, including the no-consent refusal
  (`authoring_consent_required`) and webhook delivery of `prompt_matched`.
- `verify-cli-dogfood.py`: both PASS lines. Camera preview opened the real
  webcam (`framesSeen=39`, `previewOnly=true`); workout mode correctly rejected
  `start`, `finish`, `debug done` and `--lock-mode on`.
- `verify-camera-gating.sh 8081` against the live installed hub:
  `CAMERA GATING OK` — `/dev/video0` unheld before, held during, unheld after.
- Read-only surfaces: `rfp inspect` (Claude detected, Codex not),
  `rfp site` / `rfp profile` (no secrets in output), and a real HTTPS read
  roundtrip via `rfp status|history --source remote` against Vercel. No upload
  was triggered.
- `verify-passive.py`: PASS on both isolated launches.
- `verify-daemon.py`: PASS after the time-of-day fix below.

Fixed while running:

- `qualify-software.mjs` pointed `UV_CACHE_DIR` at a **fresh empty temp dir**
  and then set `UV_OFFLINE=1`, so `detector`, `public-video` and `bundle` could
  never pass. Now defaults to `~/.cache/uv`.
- `verify-daemon.py` waited for daemon state `counting`, but the end-of-day
  warning is due whenever local minute >= `workday_end - warn`, so the stock
  18:00 default fires immediately after 17:00 local and the state never
  arrives. The disposable profile now pre-seeds `workday_end` ahead of now.
- `demo-n1.sh` claimed to expect `snapshot_captured` from `poll_events`; the
  hub emits `snapshot.executed` as an *action result*, so the snapshot half of
  the N1 gate was never actually checked. Corrected and now asserted.
- `demo-n1.sh` / `demo-n2.sh` hardcoded their ports; both now honor `PORT` /
  `DEBUG_PORT` (8085 was occupied by an unrelated service on this machine).

Defects found and fixed — 2026-09-28:

- **Hub supervisor never retried a failed start.** If port 8443 was briefly
  occupied when the app launched, it logged `hub: failed to start … honor mode
  only` once and stayed in honor mode for the life of the process — observed
  still down two minutes after the port was free, recoverable only by restart.
  This was the documented "nothing is detected" symptom: `free-hub.sh` treated
  the cause, nothing treated the stuck state.
  `HubSupervisor::start_retrying` now retries with backoff from 500 ms to a
  30 s cap, abandoning the wait the moment the app is stopping. Covered by
  `hub-client` unit test
  `a_failed_start_is_retried_instead_of_dropping_to_honor_mode_forever`.
  Verified live: with the port held, the log showed `retrying in 0.5s / 1s / 2s
  / 4s / 8s`; freeing the port brought `visionHost: up` within ~12 s with no
  restart.
- **The status message lied during that state.** `rfp camera status` reported
  `Vision hub is starting` indefinitely. `hub::unavailable_reason()` now
  distinguishes "still starting" from a failed start and reports the cause:
  `Vision hub is not running (hub io error: hub port 8443 is in use; refusing
  to terminate another process); retrying in the background`.
- **Long `REPS_APP_HOME` panicked opaquely.** `control.sock` is a Unix socket,
  so a data dir that pushed it past the 108-byte `sun_path` limit aborted with
  libstd's bare `path must be shorter than SUN_LEN`. The bind site now checks
  the length first and reports the path, the byte count, the limit and the
  remedy. It still surfaces as a Tauri setup panic — Tauri panics on any setup
  error — but it now says what to do.

These three were verified against freshly built binaries; installing them over
the running dogfood build is a separate, deliberate step
(`python3 scripts/install-dogfood.py`).

Still hands-and-eyes only: hubd killed mid-set (single restart, then honor
fallback with `verified = 0`), the physical camera LED, the phone tuning app,
jump-rope streak reset, physical rep accuracy, and the workday soak.
