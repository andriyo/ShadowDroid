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
    repo=Path(__file__).resolve().parents[1];out=args.out.resolve();out.mkdir(parents=True, mode=0o700)
    source=repo/'samples/shadowdroid-test-app';package='io.github.andriyo.shadowdroid.sample'
    plan={'schema_version':1,'task':'Build and inspect the resolved app, then verify the secondary route','source_root':str(source),'inputs':['shadowdroid/shadowdroid-agent.aar','shadowdroid/shadowdroid-agent-okhttp.aar'],
    'requirements':[{'id':'r','text':'fresh app and declared dependency plus reachable form','source':'tool acceptance fixture','checks':['build','dependencies','constraints','journey']}],
    'checks':[
    {'id':'build','adapter':{'kind':'build_install','build':{'argv':['./gradlew','--no-daemon','clean',':app:assembleDebug'],'cwd':'.','timeout_ms':180000,'apk':'app/build/outputs/apk/debug/app-debug.apk','package':package}}},
    {'id':'dependencies','depends_on':['build'],'adapter':{'kind':'resolved_dependencies','dependencies':{'argv':['./gradlew','--no-daemon'],'module':':app','configuration':'debugRuntimeClasspath','timeout_ms':180000,'required':[{'group':'org.jetbrains.kotlinx','name':'kotlinx-coroutines-android','version':'1.11.0'}]}}},
    {'id':'constraints','adapter':{'kind':'source_constraints','rules':{'files':['app/src/main/kotlin/io/github/andriyo/shadowdroid/sample/VerificationFixtureActivity.kt'],'required_patterns':['class VerificationFixtureActivity'],'forbidden_patterns':['findViewById']}}},
    {'id':'connect','depends_on':['build'],'adapter':{'kind':'connect','server_apk':str(repo/'server/app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk')}},
    {'id':'journey','depends_on':['connect'],'adapter':{'kind':'journey','journey':{'package':package,'destinations':['form'],'steps':[{'action':'start','activity':'.VerificationFixtureActivity'},{'action':'assert','target':{'by':'rid','value':package+':id/verification_title'},'text':'Verification form','destination':'form'}]}}}]}
    p=out/'plan.json';p.write_text(json.dumps(plan,indent=2))
    env=dict(os.environ,SHADOWDROID_QUIET='1');env.pop('SHADOWDROID_SESSION',None)
    prefix=[str(args.cli.resolve()),'--device',args.device,'--authority-dir',str(out/'authority')]
    for name in ['candidate','wrong-dependency']:
     if name=='wrong-dependency':
      plan['checks']=[plan['checks'][1]];plan['checks'][0].pop('depends_on');plan['checks'][0]['adapter']['dependencies']['required'][0]['version']='0.0.0';plan['requirements'][0]['checks']=['dependencies'];p=out/'wrong.json';p.write_text(json.dumps(plan))
     r=subprocess.run(prefix+['verify','run',str(p),'--out',str(out/name)],capture_output=True,text=True,env=env,timeout=360)
     (out/(name+'.output.json')).write_text(json.dumps({'code':r.returncode,'stdout':r.stdout,'stderr':r.stderr}));print(name,r.returncode,flush=True)
     assert (r.returncode==0)==(name=='candidate'),r.stdout
     report=json.loads((out/name/'report.json').read_text())
     if name=='candidate':assert report['current_edits_verified_at_run'],report
    print('PASS')


if __name__ == "__main__":
    main()
