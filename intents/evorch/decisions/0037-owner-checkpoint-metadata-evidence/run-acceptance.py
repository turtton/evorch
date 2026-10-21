import json, os, random, subprocess, time
from pathlib import Path
base=Path(__file__).resolve().parent
binary=base/'build/release'
cases=[(version,pause,rep) for rep in (1,2) for pause in (0,100) for version in ('before','after')]
random.Random(37).shuffle(cases)
with (base/'results.jsonl').open('w') as output:
    for version,pause,rep in cases:
        env=os.environ.copy();env['WRITER_PAUSE_MS']=str(pause)
        command=[str(binary/f'owner-checkpoint-bench-{version}'),'permit','5000000','20','20','15','0',str(rep)]
        start=time.monotonic()
        result=subprocess.run(command,env=env,capture_output=True,text=True,timeout=90,check=True)
        row=json.loads(result.stdout);row.update(version=version,pause_ms=pause)
        output.write(json.dumps(row)+'\n');output.flush()
        print(version,pause,rep,'writer',row['writers']['failed'],'/',row['writers']['ops'],'reader',row['readers']['failed'],'/',row['readers']['ops'],'elapsed',time.monotonic()-start,flush=True)
for pause in (0,100):
    for version in ('before','after'):
        rows=[json.loads(line) for line in (base/'results.jsonl').read_text().splitlines() if json.loads(line)['version']==version and json.loads(line)['pause_ms']==pause]
        w=sum(r['writers']['failed'] for r in rows); wn=sum(r['writers']['ops'] for r in rows)
        r=sum(r['readers']['failed'] for r in rows); rn=sum(r['readers']['ops'] for r in rows)
        print('TOTAL',version,pause,f'writer {w}/{wn}={100*w/wn:.4f}%',f'reader {r}/{rn}={100*r/rn:.4f}%',flush=True)
