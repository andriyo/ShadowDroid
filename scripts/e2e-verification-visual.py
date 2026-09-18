#!/usr/bin/env python3
"""Seeded verification adapter E2E. Requires the sample/server APKs and an explicit disposable device."""
import argparse
import json
import os
import subprocess
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--device', required=True)
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--cli', type=Path, default=Path(__file__).resolve().parents[1] / 'cli/target/debug/shadowdroid')
    args = parser.parse_args()
    repo=Path(__file__).resolve().parents[1];out=args.out.resolve();out.mkdir(parents=True, mode=0o700);source=out/'source';source.mkdir();(source/'fixture.txt').write_text('visual contract v1')
    package='io.github.andriyo.shadowdroid.sample';env=dict(os.environ,SHADOWDROID_QUIET='1');env.pop('SHADOWDROID_SESSION',None)
    prefix=[str(args.cli.resolve()),'--device',args.device,'--authority-dir',str(out/'authority')]
    def invoke(words):
     r=subprocess.run(prefix+words,text=True,capture_output=True,env=env,timeout=90)
     return r
    r=invoke(['--apk',str(repo/'server/app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk'),'connect']);assert r.returncode==0,r.stdout
    for name,broken,rotation in [('reference',False,0),('correct',False,0),('wrong-theme',True,0),('mismatch',False,1)]:
     steps=[{'action':'configure','configuration':{'night':True,'rotation':rotation,'font_scale':1.0}}, {'action':'start','activity':'.BrokenVerificationFixtureActivity' if broken else '.VerificationFixtureActivity'}, {'action':'assert','target':{'by':'rid','value':package+':id/verification_title'},'text':'Verification form','destination':'form'},{'action':'capture','name':'screen'}]
     plan={'schema_version':1,'task':name,'inputs':['fixture.txt'],'requirements':[{'id':'r','text':name,'source':'fixture','checks':['journey']}], 'checks':[{'id':'journey','adapter':{'kind':'journey','journey':{'package':package,'destinations':['form'],'steps':steps}}}]}
     if name!='reference':
      ref=out/'reference/checks/journey';meta=json.loads((ref/'screen.json').read_text());viewport=meta['before']['viewport'];bounds=next(e['bounds'] for e in meta['before']['elements'] if e.get('rid','').endswith('verification_instance'))
      spec={'reference_png':str(ref/'screen.png'),'reference_metadata':str(ref/'screen.json'),'capture_check':'journey','capture_name':'screen','max_changed_fraction':0.01,'channel_tolerance':8,'masks':[{'bounds':[0,0,viewport['w'],120],'reason':'system status area above fixture content'},{'bounds':[0,viewport['h']-100,viewport['w'],viewport['h']],'reason':'system navigation area'},{'bounds':bounds,'reason':'per-instance UUID, checked behaviorally elsewhere'}]}
      plan['checks'].append({'id':'visual','depends_on':['journey'],'adapter':{'kind':'visual_comparison','comparison':spec}});plan['requirements'][0]['checks'].append('visual')
     path=source/(name+'.json');path.write_text(json.dumps(plan,indent=2));r=invoke(['verify','run',str(path),'--out',str(out/name)]);(out/(name+'.output.json')).write_text(json.dumps({'code':r.returncode,'stdout':r.stdout,'stderr':r.stderr}));print(name,r.returncode,flush=True)
     assert (r.returncode==0)==(name in ('reference','correct')),r.stdout
     if name!='reference':
      report=json.loads((out/name/'report.json').read_text());assert report['check_statuses']['visual']=={'correct':'passed','wrong-theme':'failed','mismatch':'blocked'}[name],report
    print('PASS')


if __name__ == "__main__":
    main()
