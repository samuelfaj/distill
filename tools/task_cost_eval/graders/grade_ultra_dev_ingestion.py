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
    root = fixture_dir(argv, 'ultra_dev_ingestion')
    failures = []
    try:
        errors, validation, aggregate, csv_feed, jsonl_feed, api = import_fresh(root, 'errors', 'validation', 'aggregate', 'csv_feed', 'jsonl_feed', 'api')
    except Exception as error:
        return finish(['import: ' + repr(error)])
    def error_result(call):
        try:
            call()
        except errors.FeedError as error:
            return error.code, error.row, error.field, str(error)
        raise AssertionError('expected FeedError')
    def bad(label, call, code, row, field):
        attempt(failures,label,lambda: error_result(call),(code,row,field,f'{code} at row {row}: {field}'))
    a = dict(id='ev-a',warehouse='A',sku='pen',delta=5)
    b = dict(id='ev-b',warehouse='A',sku='pen',delta=-5)
    c = dict(id='ev-c',warehouse='B',sku='pad',delta=7)
    csv_text = 'sku,delta,id,warehouse,comment\r\npen,+5, ev-a , A ,ignored\r\n\r\n"pad",7,ev-c,B,"two, parts"\r\n'
    jsonl_text = '\n'+json.dumps(a)+'\n\n'+json.dumps(b)+'\n'
    attempt(failures,'CSV normalization',lambda: csv_feed.parse(csv_text),[a,c])
    attempt(failures,'JSONL normalization',lambda: jsonl_feed.parse(jsonl_text),[a,b])
    attempt(failures,'CSV routing',lambda: api.load('csv',csv_text),[a,c])
    attempt(failures,'JSONL routing',lambda: api.load('jsonl',jsonl_text),[a,b])
    feeds = [('csv',csv_text),('jsonl',jsonl_text),('json',json.dumps([c]))]
    expected = [{'warehouse':'A','sku':'pen','delta':0},{'warehouse':'B','sku':'pad','delta':7}]
    original = copy.deepcopy(feeds)
    attempt(failures,'cross format dedup',lambda: api.combine(feeds),expected)
    attempt(failures,'repeat no global state',lambda: api.combine(list(reversed(feeds))),expected)
    records = [c,a,b,a]
    saved = copy.deepcopy(records)
    attempt(failures,'aggregate public',lambda: aggregate.summarize(records),expected)
    attempt(failures,'records unchanged',lambda: records,saved)
    attempt(failures,'feeds unchanged',lambda: feeds,original)
    attempt(failures,'legacy JSON',lambda: api.load('json',json.dumps([a,b])),[a,b])
    for fmt,text in [('json','[]'),('jsonl','\n  \n'),('csv','id,warehouse,sku,delta\n')]:
        attempt(failures,fmt+' empty',lambda: api.load(fmt,text),[])
    bad('missing header',lambda: api.load('csv','warehouse,sku,delta\nA,pen,2'),'header',0,'id')
    bad('duplicate header',lambda: api.load('csv','id,id,warehouse,sku,delta\n'),'syntax',0,'csv')
    bad('CSV syntax',lambda: api.load('csv','id,warehouse,sku,delta\n"oops'),'syntax',0,'csv')
    bad('CSV cells',lambda: api.load('csv','id,warehouse,sku,delta\na,A,pen,3\n\nb,A'),'record',2,'')
    bad('CSV ordinal',lambda: api.load('csv','id,warehouse,sku,delta\na,A,pen,3\n\nb,A,pen,1.5'),'field',2,'delta')
    bad('JSONL physical line',lambda: api.load('jsonl',json.dumps(a)+'\n\n{bad}'),'syntax',3,'jsonl')
    bad('JSONL record',lambda: api.load('jsonl','\n[]'),'record',2,'')
    bad('unknown format',lambda: api.load('yaml',''),'format',0,'format')
    bad('JSON syntax',lambda: api.load('json','['),'syntax',0,'json')
    bad('JSON non-array',lambda: api.load('json','{}'),'record',0,'')
    for value in (True, 1.5, ' 2', '2.0', '٢', None):
        bad('delta validation',lambda: validation.normalize({**a,'delta':value},9),'field',9,'delta')
    bad('validation order',lambda: validation.normalize({'id':'','delta':True},4),'field',4,'id')
    bad('duplicate conflict',lambda: api.combine([('json',json.dumps([a])),('jsonl',json.dumps({**a,'delta':6}))]),'conflict',2,'id')
    with tempfile.TemporaryDirectory() as tmp:
        args = []
        for i,(fmt,text) in enumerate(feeds):
            path = Path(tmp)/f'feed:{i}.txt'
            path.write_text(text)
            args.append(f'{fmt}:{path}')
        env = {**os.environ,'PYTHONDONTWRITEBYTECODE':'1'}
        run = subprocess.run([sys.executable,str(root/'cli.py'),*args],capture_output=True,text=True,env=env,timeout=10)
        attempt(failures,'CLI status',lambda: run.returncode,0)
        attempt(failures,'CLI combined JSON',lambda: json.loads(run.stdout),expected)
        run = subprocess.run([sys.executable,str(root/'cli.py'),args[0],f'json:{Path(tmp)/"absent"}'],capture_output=True,text=True,env=env,timeout=10)
        attempt(failures,'CLI atomic failure',lambda: (run.returncode,run.stdout,bool(run.stderr)),(2,'',True))
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
