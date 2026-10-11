#!/usr/bin/env python3
import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from _par_common import attempt, finish, fixture_dir, import_fresh


def main(argv):
    root = fixture_dir(argv,'ultra_heldout_config')
    failures = []
    try:
        schema, json_source, env_source, merge, export, api = import_fresh(root,'schema','json_source','env_source','merge','export','api')
    except Exception as error:
        return finish(['import: '+repr(error)])
    defaults = {'server':{'host':'localhost','port':8080},'features':{'debug':False,'tags':['base']},'paths':{'root':'.'}}
    attempt(failures,'legacy defaults',lambda: api.default_config(),defaults)
    attempt(failures,'legacy validation',lambda: schema.validate(defaults),None)
    attempt(failures,'JSON partial',lambda: json_source.parse('{"server":{"port":9000},"features":null}'),{'server':{'port':9000},'features':None})
    env = {'APP__SERVER__PORT':' +9200 ','APP__FEATURES__DEBUG':' TRUE ','APP__FEATURES__TAGS':' live, ,fast,live ','APP__PATHS__ROOT':' /srv ','HOME':'ignored','APP_PORT':'ignored'}
    saved_env = copy.deepcopy(env)
    parsed_env = {'server':{'port':9200},'features':{'debug':True,'tags':['live','fast','live']},'paths':{'root':'/srv'}}
    attempt(failures,'environment parser',lambda: env_source.parse(env),parsed_env)
    patch = {'features':{'tags':['one'],'debug':True},'server':{'host':'example'}}
    saved_patch = copy.deepcopy(patch)
    first = {**defaults,'features':{'tags':['one'],'debug':True},'server':{'host':'example','port':8080}}
    attempt(failures,'recursive overlay',lambda: merge.overlay(defaults,patch),first)
    attempt(failures,'reset leaf',lambda: merge.overlay(first,{'features':{'tags':None}}),{**first,'features':{'tags':['base'],'debug':True}})
    attempt(failures,'reset section',lambda: merge.overlay(first,{'features':None}),{**first,'features':defaults['features']})
    def detached():
        result = merge.overlay(defaults,patch)
        result['features']['tags'].append('caller')
        return defaults,patch
    attempt(failures,'overlay snapshots',detached,({'server':{'host':'localhost','port':8080},'features':{'debug':False,'tags':['base']},'paths':{'root':'.'}},saved_patch))
    texts = ['{"server":{"host":"internal","port":9000},"features":{"tags":["old"],"debug":true}}',
             '{"server":{"port":null},"features":null,"paths":{"root":"/data"}}']
    expected = {'server':{'host':'internal','port':9200},'features':{'debug':True,'tags':['live','fast','live']},'paths':{'root':'/srv'}}
    attempt(failures,'deterministic precedence integration',lambda: api.resolve(texts,env),expected)
    attempt(failures,'reset resolves default',lambda: api.resolve(texts),{'server':{'host':'internal','port':8080},'features':defaults['features'],'paths':{'root':'/data'}})
    attempt(failures,'later JSON wins',lambda: api.resolve(['{"server":{"port":9001}}','{"server":{"port":9002}}'])['server']['port'],9002)
    attempt(failures,'environment unchanged',lambda: env,saved_env)
    def defaults_detached():
        result = api.resolve()
        result['features']['tags'].append('mutation')
        return api.resolve()
    attempt(failures,'independent defaults',defaults_detached,defaults)
    json_result = json.dumps(expected,sort_keys=True,separators=(',',':'))+'\n'
    env_result = 'APP__FEATURES__DEBUG=true\nAPP__FEATURES__TAGS=live,fast,live\nAPP__PATHS__ROOT=/srv\nAPP__SERVER__HOST=internal\nAPP__SERVER__PORT=9200\n'
    attempt(failures,'canonical JSON export',lambda: export.render(expected),json_result)
    attempt(failures,'deterministic env export',lambda: export.render(expected,'env'),env_result)
    attempt(failures,'JSON roundtrip',lambda: api.resolve([export.render(expected)]),expected)
    attempt(failures,'env roundtrip',lambda: api.resolve(env=dict(line.split('=',1) for line in env_result.splitlines())),expected)
    attempt(failures,'empty tags',lambda: api.resolve(env={'APP__FEATURES__TAGS':''})['features']['tags'],[])
    for text in ('[]','null','{','{"other":{}}','{"server":{"missing":1}}','{"server":{"port":true}}','{"features":{"debug":1}}','{"features":{"tags":["bad,tag"]}}','{"paths":{"root":" "}}'):
        attempt(failures,'invalid JSON import',lambda: json_source.parse(text),raises=ValueError)
    for mapping in ({'APP__UNKNOWN':'x'},{'APP__FEATURES__DEBUG':'yes'},{'APP__SERVER__PORT':'1.2'},{'APP__SERVER__PORT':'65536'},{'APP__SERVER__PORT':'٢'},{'APP__SERVER__HOST':True}):
        attempt(failures,'invalid environment',lambda: env_source.parse(mapping),raises=ValueError)
    attempt(failures,'invalid overlay',lambda: merge.overlay(defaults,{'features':{'extra':False}}),raises=ValueError)
    attempt(failures,'invalid full config',lambda: schema.validate({'server':{}}),raises=ValueError)
    attempt(failures,'unknown export',lambda: export.render(defaults,'toml'),raises=ValueError)
    with tempfile.TemporaryDirectory() as tmp:
        paths = []
        for i,text in enumerate(texts):
            path = Path(tmp)/f'{i}.json'
            path.write_text(text)
            paths.extend(['--json',str(path)])
        path = Path(tmp)/'env.json'
        path.write_text(json.dumps(env))
        runtime_env = {**os.environ,'PYTHONDONTWRITEBYTECODE':'1','APP__SERVER__PORT':'10000'}
        run = subprocess.run([sys.executable,str(root/'cli.py'),*paths,'--env',str(path),'--format','env'],capture_output=True,text=True,env=runtime_env,timeout=10)
        attempt(failures,'CLI precedence and exact output',lambda: (run.returncode,run.stdout),(0,env_result))
        run = subprocess.run([sys.executable,str(root/'cli.py')],capture_output=True,text=True,env=runtime_env,timeout=10)
        attempt(failures,'CLI defaults no implicit env',lambda: (run.returncode,run.stdout),(0,json.dumps(defaults,sort_keys=True,separators=(',',':'))+'\n'))
        path.write_text('{"APP__SERVER__PORT":"bad"}')
        run = subprocess.run([sys.executable,str(root/'cli.py'),'--env',str(path)],capture_output=True,text=True,env=runtime_env,timeout=10)
        attempt(failures,'CLI failure',lambda: (run.returncode,run.stdout,bool(run.stderr)),(2,'',True))
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
