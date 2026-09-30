#!/usr/bin/env python3
"""Exercise the installed daemon with a disposable profile and no cloud config."""
import json, os, signal, sqlite3, subprocess, tempfile, time
from datetime import datetime
from pathlib import Path
home=Path.home(); app=home/'.local/lib/rfp/bin/app'; cli=home/'.local/bin/rfp'
def service(action): subprocess.run(['systemctl','--user',action,'rfp.service'],check=True)
proc=None
with tempfile.TemporaryDirectory(prefix='rfp-watcher-test-') as folder:
    profile=Path(folder); (profile/'mode.json').write_text('"workout"')
    # The end-of-day warning is due whenever local minute >= workday_end - warn,
    # so the stock 18:00 default fires instantly after 17:00 and the daemon never
    # reaches 'counting'. Park the deadline ahead of now; the test arms the warning
    # itself later with --end 00:00.
    # ponytail: no date in the setting, so a run starting after 23:58 still warns early.
    ahead=min(datetime.now().hour*60+datetime.now().minute+90,1439)
    with sqlite3.connect(profile/'reps.sqlite') as db:
        db.execute('create table settings(key text primary key,value text not null)')
        db.execute('insert into settings values(?,?)',('workday_end',f'{ahead//60:02d}:{ahead%60:02d}'))
    env=dict(os.environ, REPS_APP_HOME=folder)
    env.setdefault('DISPLAY',':0')
    def command(*args): return json.loads(subprocess.check_output([str(cli),*args,'--json'],env=env,text=True))
    def wait_for(predicate):
        deadline=time.monotonic()+15
        while time.monotonic()<deadline:
            try:
                value=command('inspect')
                if predicate(value): return value
            except (subprocess.CalledProcessError,json.JSONDecodeError): pass
            time.sleep(.25)
        raise AssertionError('Daemon condition did not arrive')
    def launch(log):
        return subprocess.Popen([str(app),'--background'],env=env,stdout=log,stderr=log,start_new_session=True)
    def stop():
        if proc is not None:
            try: os.killpg(proc.pid,signal.SIGTERM)
            except ProcessLookupError: pass
            try: proc.wait(timeout=5)
            except subprocess.TimeoutExpired: os.killpg(proc.pid,signal.SIGKILL);proc.wait()
    service('stop')
    try:
        with (profile/'daemon.log').open('w') as log:
            proc=launch(log)
            initial=wait_for(lambda v:v.get('state')=='counting')
            assert initial['agents']['codex'] or initial['agents']['claude']
            command('snooze','--minutes','5')
            command('workday','--end','00:00','--warn-minutes','60')
            time.sleep(6)
            assert command('inspect')['state']=='snoozed'
            assert command('inspect')['lastDayWarning']==''
            command('skip')
            warned=wait_for(lambda v:v.get('state')=='reminder' and bool(v.get('notice')))
            assert 'might miss' in warned['notice']
            assert not command('history')['records']
            stop();proc=None
        with (profile/'restart.log').open('w') as log:
            proc=launch(log)
            resumed=wait_for(lambda v:v.get('fresh') is True)
            time.sleep(6)
            assert resumed['lastDayWarning']==warned['lastDayWarning']
            assert not command('history')['records']
            stop();proc=None
        first=(profile/'daemon.log').read_text();second=(profile/'restart.log').read_text()
        assert first.count('[RFP REMINDER] Your workday')==1
        assert '[RFP REMINDER] Your workday' not in second
        assert 'Desktop notification unavailable' not in first
        assert not (profile/'upload.json').exists()
        print('PASS: actual coding agents detected; snooze respected; end-of-day screen and notification triggered; restart deduplicated warning; no workouts saved or uploaded.')
    finally:
        stop();service('start')
