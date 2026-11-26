#!/usr/bin/env python3
"""Exercise installed CLI controls. Test credit is confined to Debug's temp store.
Restores Workout mode; requires the real user service to be idle and running.
"""
import json, subprocess, time, tempfile
from pathlib import Path
cli=str(Path.home()/'.local/bin/rfp')
def run(*args):
    p=subprocess.run([cli,*args,'--json'],capture_output=True,text=True,timeout=60)
    if p.returncode:raise RuntimeError(f'{args}: {p.stderr}')
    return json.loads(p.stdout)
def reject(*args):
    p=subprocess.run([cli,*args,'--json'],capture_output=True,text=True,timeout=20)
    assert p.returncode, args
before=run('history')['records']
for _ in range(40):
    initial=run('status').get('runtime')
    if initial:break
    time.sleep(.25)
# V2 rests in IDLE until a session starts; pre-V2 builds rest in CODING.
resting=initial and initial['snapshot']['phase']
assert resting in ('IDLE','CODING'),initial
try:
    debug=run('mode','debug');assert debug['mode']=='debug',debug
    assert run('debug','exercises')
    reject('start')
    run('debug','start','--exercise','squat')
    run('debug','step','--value','2')
    assert run('status')['runtime']['snapshot']['progress']['value']==2
    reject('mode','workout')
    run('debug','done')
    run('finish','--weight','0')
    assert len(run('debug','history')['records'])==1
    assert run('history')['records']==before
    reject('finish')
    run('debug','stop')
    routine=run('routine')
    with tempfile.TemporaryDirectory(prefix='rfp-cli-routine-') as d:
        p=Path(d)/'routine.json';p.write_text(json.dumps(routine))
        assert run('routine','--file',str(p))==routine
    videos=run('debug','videos')
    assert videos,'Bundled fixture missing'
    sample=videos[0]
    run('debug','video','--exercise',sample['exercise'],'--file',sample['path'])
    for _ in range(30):
        time.sleep(.25)
        video_state=run('status')['runtime']['display']
        if video_state['framesSeen']>0:break
    assert video_state['framesSeen']>0,video_state
    run('debug','video-stop')
    run('camera','list');run('camera','settings')
    reject('camera','set','--rotation','45')
    run('camera','preview')
    time.sleep(3)
    camera=run('camera','status')
    print('Camera preview:',json.dumps(camera))
    assert camera['camera']=='open' and camera['display']['framesSeen']>0,camera
    assert run('status')['runtime']['snapshot']['phase'] in ('IDLE','CODING') # preview never starts a workout
    assert len(run('debug','history')['records'])==1
    run('camera','stop')
    run('display','--window','main','--visible','on')
    print('PASS: CLI mode switch, debug simulation/completion, routine roundtrip, validation, camera preview controls, and display controls.')
finally:
    try:run('debug','stop')
    finally:run('mode','workout')
assert run('history')['records']==before
reject('debug','done');reject('settings','--lock-mode','on')
print('PASS: restored Workout mode; real history unchanged; debug simulation and terminal lockout blocked in Workout mode.')
