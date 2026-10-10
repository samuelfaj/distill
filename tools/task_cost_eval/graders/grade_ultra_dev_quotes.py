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
    root = fixture_dir(argv, 'ultra_dev_quotes')
    failures = []
    try:
        catalog, rates, pricing, cache, service, invoice = import_fresh(root, 'catalog', 'rates', 'pricing', 'cache', 'service', 'invoice')
    except Exception as error:
        return finish(['import: '+repr(error)])
    source = {'pen': {'name':'Pen','cents':101,'tags':['office']}, 'pad':{'name':'Pad','cents':499,'tags':[]}}
    rate_source = {'USD':{'numerator':1,'denominator':1}, 'EUR':{'numerator':3,'denominator':2}}
    products = catalog.Catalog(source)
    fx = rates.RateTable(rate_source)
    source['pen']['cents'] = 9900
    source['pen']['tags'].append('changed')
    rate_source['EUR']['numerator'] = 100
    attempt(failures,'catalog owned snapshot',lambda: products.get('pen'),{'name':'Pen','cents':101,'tags':['office']})
    attempt(failures,'rates owned snapshot',lambda: fx.get('EUR'),{'numerator':3,'denominator':2})
    product = products.get('pad')
    product['tags'].append('caller')
    product['cents'] = 0
    rate = fx.get('USD')
    rate['denominator'] = 9
    attempt(failures,'catalog read snapshot',lambda: products.get('pad'),{'name':'Pad','cents':499,'tags':[]})
    attempt(failures,'rates read snapshot',lambda: fx.get('USD'),{'numerator':1,'denominator':1})
    quotes = service.QuoteService(products,fx)
    order = {'id':'checkout','items':[{'sku':'pen','quantity':1},{'sku':'pad','quantity':2},{'sku':'pen','quantity':2}]}
    saved = copy.deepcopy(order)
    original = {'order_id':'checkout','currency':'USD','lines':[{'sku':'pen','quantity':1,'usd_cents':101},{'sku':'pad','quantity':2,'usd_cents':998},{'sku':'pen','quantity':2,'usd_cents':202}], 'total_cents':1301}
    attempt(failures,'initial quote structure',lambda: quotes.quote(order),original)
    attempt(failures,'currency half up',lambda: quotes.quote(order,'EUR')['total_cents'],1952)
    first = quotes.quote(order)
    first['lines'][0]['usd_cents'] = -88
    first['metadata'] = ['intruder']
    attempt(failures,'quote detached',lambda: quotes.quote(order),original)
    attempt(failures,'invoice annotation',lambda: invoice.build(quotes,order,note='paid'),{**original,'invoice_note':'paid'})
    attempt(failures,'invoice no cache poisoning',lambda: quotes.quote(order),original)
    attempt(failures,'different invoice',lambda: invoice.build(quotes,order,note='pending'),{**original,'invoice_note':'pending'})
    before_update = quotes.quote(order)
    products.set_price('pen',201)
    attempt(failures,'price refresh',lambda: quotes.quote(order)['total_cents'],1601)
    attempt(failures,'older quote stable',lambda: before_update,original)
    fx.set_rate('EUR',2,1)
    attempt(failures,'rate refresh',lambda: quotes.quote(order,'EUR')['total_cents'],3202)
    altered = {'id':'checkout','items':[{'sku':'pad','quantity':1}]}
    attempt(failures,'same id different items',lambda: quotes.quote(altered)['total_cents'],499)
    attempt(failures,'input unchanged',lambda: order,saved)
    attempt(failures,'empty order',lambda: quotes.quote({'id':'empty','items':[]})['total_cents'],0)
    attempt(failures,'large exact conversion',lambda: pricing.convert(10**18+1,{'numerator':7,'denominator':3}),2333333333333333336)
    fx.set_rate('EUR',1,2)
    attempt(failures,'sum before rounding',lambda: quotes.quote({'id':'round','items':[{'sku':'pen','quantity':1},{'sku':'pen','quantity':1}]},'EUR')['total_cents'],201)
    for invalid in (0,-1,True,1.5):
        attempt(failures,'quantity validation even cached id',lambda: quotes.quote({'id':'checkout','items':[{'sku':'pad','quantity':invalid}]}),raises=ValueError)
    attempt(failures,'unknown product',lambda: quotes.quote({'id':'missing','items':[{'sku':'missing','quantity':1}]}),raises=KeyError)
    attempt(failures,'unknown currency',lambda: quotes.quote(order,'XXX'),raises=KeyError)
    revision = products.revision
    for invalid in (-1,True,1.5):
        attempt(failures,'price validation',lambda: products.set_price('pen',invalid),raises=ValueError)
    attempt(failures,'invalid price atomic',lambda: (products.revision,products.get('pen')['cents']),(revision,201))
    revision = fx.revision
    for numerator,denominator in [(0,1),(1,0),(True,1),(1,False),(1.5,2)]:
        attempt(failures,'rate validation',lambda: fx.set_rate('EUR',numerator,denominator),raises=ValueError)
    attempt(failures,'invalid rate atomic',lambda: (fx.revision,fx.get('EUR')),(revision,{'numerator':1,'denominator':2}))
    snapshots = cache.QuoteCache()
    attempt(failures,'cache miss',lambda: snapshots.get('none'),None)
    value = {'lines':[{'v':1}]}
    snapshots.put('a',value)
    value['lines'][0]['v'] = 9
    attempt(failures,'cache write snapshot',lambda: snapshots.get('a'),{'lines':[{'v':1}]})
    read = snapshots.get('a')
    read['lines'].clear()
    attempt(failures,'cache read snapshot',lambda: snapshots.get('a'),{'lines':[{'v':1}]})
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp)/'checkout.json'
        path.write_text(json.dumps({'catalog':{'x':{'name':'X','cents':3,'tags':[]}},'rates':{'USD':{'numerator':1,'denominator':1},'EUR':{'numerator':1,'denominator':2}},'order':{'id':'cli','items':[{'sku':'x','quantity':1}]}}))
        env = {**os.environ,'PYTHONDONTWRITEBYTECODE':'1'}
        run = subprocess.run([sys.executable,str(root/'cli.py'),str(path),'--currency','EUR','--note','ok'],capture_output=True,text=True,env=env,timeout=10)
        attempt(failures,'CLI status',lambda: run.returncode,0)
        attempt(failures,'CLI invoice',lambda: json.loads(run.stdout),{'order_id':'cli','currency':'EUR','lines':[{'sku':'x','quantity':1,'usd_cents':3}],'total_cents':2,'invoice_note':'ok'})
        run = subprocess.run([sys.executable,str(root/'cli.py'),str(path),'--currency','XXX'],capture_output=True,text=True,env=env,timeout=10)
        attempt(failures,'CLI error',lambda: (run.returncode,run.stdout,bool(run.stderr)),(2,'',True))
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
