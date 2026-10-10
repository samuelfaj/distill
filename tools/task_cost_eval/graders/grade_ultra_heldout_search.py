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
    root = fixture_dir(argv, 'ultra_heldout_search')
    failures = []
    try:
        documents, tokenizer, index, search, filters, api = import_fresh(root,'documents','tokenizer','index','search','filters','api')
    except Exception as error:
        return finish(['import: '+repr(error)])
    docs = [dict(id='z',title='Python Straße',body='tools tools python',tags=['dev','guide'],rating=4,archived=False),
            dict(id='a',title='Tools',body='python strasse tools',tags=['dev'],rating=2,archived=False),
            dict(id='b',title='Python Tools',body='tools',tags=['dev','guide'],rating=4,archived=True),
            dict(id='c',title='Guide',body='pythonic tools',tags=['guide'],rating=-1,archived=False),
            dict(id='d',title='Python',body='tools',tags=['dev','guide'],rating=4,archived=False)]
    saved = copy.deepcopy(docs)
    attempt(failures,'tokenizer unchanged',lambda: tokenizer.words('Straße STRASSE foo_bar １２3'),['strasse','strasse','foo','bar','１２3'])
    def ranking(text):
        return search.search(index.build(docs),text)
    attempt(failures,'weighted AND ranking',lambda: ranking('PYTHON tools python'),[{'id':'b','score':5},{'id':'z','score':5},{'id':'a','score':4},{'id':'d','score':3}])
    attempt(failures,'unicode matching',lambda: ranking('STRASSE'),[{'id':'z','score':2},{'id':'a','score':1}])
    attempt(failures,'no substring match',lambda: ranking('pyth'),[])
    attempt(failures,'AND missing term',lambda: ranking('python missing'),[])
    attempt(failures,'empty ranking',lambda: ranking('___!?'),[{'id':key,'score':0} for key in ('a','b','c','d','z')])
    attempt(failures,'empty index',lambda: search.search(index.build([]),'tools'),[])
    snapshot_source = copy.deepcopy(docs)
    def snapshot_check():
        built = index.build(snapshot_source)
        snapshot_source[0]['body'] = 'gone'
        snapshot_source.clear()
        return search.search(built,'strasse')
    attempt(failures,'index snapshot',snapshot_check,[{'id':'z','score':2},{'id':'a','score':1}])
    filtered = [docs[4],docs[0]]
    attempt(failures,'filter ALL tags and inclusive rating',lambda: filters.select(docs,['guide','dev','dev'],4),filtered)
    attempt(failures,'include archived',lambda: [d['id'] for d in filters.select(docs,['dev','guide'],4,True)],['b','d','z'])
    attempt(failures,'negative rating valid',lambda: [d['id'] for d in filters.select(docs,min_rating=-1)],['a','c','d','z'])
    attempt(failures,'case sensitive tags',lambda: filters.select(docs,['DEV']),[])
    for value in (True,1.5,'4'):
        attempt(failures,'rating validation',lambda: filters.select(docs,min_rating=value),raises=ValueError)
    result = {'total':2,'hits':[{**docs[0],'score':5},{**docs[4],'score':3}]}
    attempt(failures,'API integration',lambda: api.query(docs,'python tools',['guide','dev'],4),result)
    attempt(failures,'API pagination after filter',lambda: api.query(docs,'python tools',['guide','dev'],4,False,1,1),{'total':2,'hits':[result['hits'][1]]})
    attempt(failures,'API zero limit',lambda: api.query(docs,limit=0),{'total':4,'hits':[]})
    attempt(failures,'API beyond total',lambda: api.query(docs,offset=99),{'total':4,'hits':[]})
    attempt(failures,'all documents default',lambda: [d['id'] for d in api.query(docs)['hits']],['a','c','d','z'])
    attempt(failures,'legacy list includes archived',lambda: api.list_documents(docs),sorted(saved,key=lambda d:d['id']))
    def detach_check():
        output = api.query(docs,'python tools')
        output['hits'][0]['tags'].append('changed')
        output['hits'][0]['body'] = 'changed'
        return api.query(docs,'python tools',['guide','dev'],4)
    attempt(failures,'API snapshots',detach_check,result)
    def filter_detach():
        output = filters.select(docs)
        output[0]['tags'].clear()
        return docs
    attempt(failures,'filter snapshots',filter_detach,saved)
    attempt(failures,'input unchanged',lambda: docs,saved)
    for offset,limit in [(-1,None),(True,None),(0,-1),(0,False),(0,1.2)]:
        attempt(failures,'bounds validation',lambda: api.query(docs,offset=offset,limit=limit),raises=ValueError)
    attempt(failures,'duplicate validation',lambda: api.query(docs+[docs[0]]),raises=ValueError)
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp)/'library.json'
        path.write_text(json.dumps(docs))
        env = {**os.environ,'PYTHONDONTWRITEBYTECODE':'1'}
        run = subprocess.run([sys.executable,str(root/'cli.py'),str(path),'--query','python tools','--tag','dev','--tag','guide','--min-rating','4','--offset','1','--limit','1'],capture_output=True,text=True,env=env,timeout=10)
        attempt(failures,'CLI status',lambda: run.returncode,0)
        attempt(failures,'CLI result',lambda: json.loads(run.stdout),{'total':2,'hits':[result['hits'][1]]})
        run = subprocess.run([sys.executable,str(root/'cli.py'),str(path),'--limit','-1'],capture_output=True,text=True,env=env,timeout=10)
        attempt(failures,'CLI failure',lambda: (run.returncode,run.stdout,bool(run.stderr)),(2,'',True))
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
