# Configuration

The current desktop uses SQLite settings and an optional `routine.json`, rather
than the retired Python `config.toml` file.

| Setting | Location / behavior |
|---|---|
| Data directory | `~/.local/share/rfp`; override with `REPS_APP_HOME` |
| Coding timer | Persisted `work_minutes`, default 25; `REPS_WORK_MINUTES` overrides for testing |
| Daily routine | `routine.json` in the data directory; otherwise the bundled routine |
| Detector presets | Embedded `app/src-tauri/resources/exercise_specs.json`; source edits need a rebuild |
| Activated movement rules | Stored by the hub; selected at workout start and pinned for that workout |

Example local routine:

```json
{
  "lifts": [
    { "exercise": "squat", "sets": 3, "reps": 5 },
    { "exercise": "my-curl", "label": "My curl", "sets": 2, "reps": 8 }
  ]
}
```

Restart after editing. Custom IDs such as `my-curl` need an active evaluated
version. Invalid routine loading reports an error and falls back to rotation;
check the prescribed exercise after restart. See the
[bundled routine](../../../app/src-tauri/resources/routine.json) for conditioning
and stretch entries, and [tuning](adding-exercises.md) for detection settings.
