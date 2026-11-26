#!/usr/bin/env python3
"""Probe the installed desktop with disposable data; no camera or real workouts."""
import json, os, signal, sqlite3, subprocess, tempfile, time
from pathlib import Path
binary=Path.home()/'.local/lib/rfp/bin/app'
with tempfile.TemporaryDirectory(prefix='rfp-passive-') as temporary:
    home=Path(temporary); (home/'mode.json').write_text('"workout"')
    until=time.time()+900
    with sqlite3.connect(home/'reps.sqlite') as db:
        db.execute('create table settings(key text primary key,value text not null)')
        db.executemany('insert into settings values(?,?)',[('snooze_until',str(until)),('work_minutes','1')])
    for attempt in range(2):
        (home/'status.json').unlink(missing_ok=True)
        with (home/'probe.log').open('w') as log:
            child=subprocess.Popen([str(binary),'--background'],env={**os.environ,'REPS_APP_HOME':str(home),'REPS_HUB_DISABLED':'1'},stdout=log,stderr=log,start_new_session=True)
            try:
                deadline=time.monotonic()+20
                while not (home/'status.json').exists() and time.monotonic()<deadline:
                    if child.poll() is not None:raise RuntimeError((home/'probe.log').read_text())
                    time.sleep(.2)
                first=json.loads((home/'status.json').read_text())
                if first['phase']=='IDLE' and attempt==0:
                    # V2 rests in IDLE and starting a session ends a snooze: begin
                    # the session, then snooze it through the CLI.
                    rfp=lambda *a:subprocess.run([str(Path.home()/'.local/bin/rfp'),*a,'--json'],env={**os.environ,'REPS_APP_HOME':str(home)},check=True,capture_output=True,timeout=30)
                    rfp('start');rfp('snooze','--minutes','15')
                    with sqlite3.connect(home/'reps.sqlite') as db:
                        until=float(db.execute("select value from settings where key='snooze_until'").fetchone()[0])
                    assert until>time.time()
                    time.sleep(5.5)
                    first=json.loads((home/'status.json').read_text())
                # V2 restarts into IDLE (no session survives a restart); pre-V2 into CODING.
                assert first['phase'] in ('CODING','IDLE') and first['lockMode'] is False, first
                time.sleep(5.5)
                second=json.loads((home/'status.json').read_text())
                assert abs(first['remainingSeconds']-second['remainingSeconds'])<1, (first,second)
                with sqlite3.connect(home/'reps.sqlite') as db:
                    assert float(db.execute("select value from settings where key='snooze_until'").fetchone()[0])==until
                    assert db.execute('select count(*) from exercise_history').fetchone()[0]==0
                print(f'PASS launch {attempt+1}: snooze holds timer, no workout credit, phase={second["phase"]} agents={second.get("agents","n/a")}',flush=True)
            finally:
                os.killpg(child.pid,signal.SIGTERM); child.wait(timeout=10)
print('PASS snooze persisted across desktop restart; normal app and history untouched.')
