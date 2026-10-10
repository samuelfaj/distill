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
    root = fixture_dir(argv, 'ultra_dev_reports')
    failures = []
    try:
        orders, revenue, backlog, aging, registry, service = import_fresh(root, 'orders', 'revenue', 'backlog', 'aging', 'registry', 'service')
    except Exception as error:
        return finish(['import: ' + repr(error)])
    def order(oid, region, placed, status, items):
        return dict(id=oid, region=region, placed=placed, status=status, items=[dict(sku=s, quantity=q, shipped=h, unit_cents=p) for s,q,h,p in items])
    data = [order('b', 'west', '2026-03-25', 'open', [('pen', 4, 1, 150), ('pen', 2, 0, 150)]),
            order('a', 'east', '2026-03-24', 'open', [('pen', 3, 2, 200)]),
            order('c', 'west', '2026-03-02', 'open', [('pad', 5, 0, 80)]),
            order('d', 'east', '2026-03-01', 'open', [('pad', 2, 0, 80)]),
            order('e', 'north', '2026-04-04', 'open', [('clip', 2, 0, 5)]),
            order('f', 'east', '2026-03-01', 'shipped', [('pen', 2, 2, 250)]),
            order('g', 'north', '2026-03-01', 'cancelled', [('pad', 20, 3, 900)]),
            order('h', 'south', '2026-03-01', 'open', [('pen', 1, 1, 50)])]
    original = copy.deepcopy(data)
    expected = {
        'totals': {'orders': 8, 'ordered_units': 41},
        'revenue': [{'region': 'east', 'units': 4, 'revenue_cents': 900}, {'region': 'south', 'units': 1, 'revenue_cents': 50}, {'region': 'west', 'units': 1, 'revenue_cents': 150}],
        'backlog': [{'sku': 'clip', 'units': 2, 'order_ids': ['e']}, {'sku': 'pad', 'units': 7, 'order_ids': ['c','d']}, {'sku': 'pen', 'units': 6, 'order_ids': ['a','b']}],
        'aging': [{'bucket':'0-7','orders':2,'units':7}, {'bucket':'8-30','orders':2,'units':6}, {'bucket':'31+','orders':1,'units':2}]
    }
    for name, module in [('revenue',revenue),('backlog',backlog),('aging',aging)]:
        attempt(failures, name+' public', lambda: module.report(data, '2026-04-01'), expected[name])
    for name, result in expected.items():
        attempt(failures, name+' integration', lambda: service.render(name, data, '2026-04-01'), result)
        attempt(failures, name+' input order', lambda: service.render(name, list(reversed(data)), '2026-04-01'), result)
    attempt(failures, 'input unchanged', lambda: data, original)
    detached = orders.normalize(data)
    detached[0]['items'][0]['quantity'] = 999
    attempt(failures, 'normalization detached', lambda: data, original)
    for name in ('revenue','backlog'):
        attempt(failures, name+' empty', lambda: service.render(name, []), [])
    attempt(failures, 'aging empty', lambda: service.render('aging', [], '2026-04-01'), [{'bucket':v,'orders':0,'units':0} for v in ('0-7','8-30','31+')])
    for value in (None, 'bad-date'):
        attempt(failures, 'date validation', lambda: aging.report([], value), raises=ValueError)
    attempt(failures, 'unknown report', lambda: service.render('missing', []), raises=ValueError)
    attempt(failures, 'duplicate id', lambda: service.render('totals', data + [data[0]]), raises=ValueError)
    invalid = copy.deepcopy(data)
    invalid[0]['items'][0]['shipped'] = 20
    attempt(failures, 'existing quantity validation', lambda: service.render('revenue', invalid), raises=ValueError)
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp)/'orders.json'
        path.write_text(json.dumps(data))
        env = {**os.environ, 'PYTHONDONTWRITEBYTECODE':'1'}
        for name, result in expected.items():
            run = subprocess.run([sys.executable, str(root/'cli.py'), name, str(path), '--as-of', '2026-04-01'], capture_output=True, text=True, env=env, timeout=10)
            attempt(failures, name+' CLI status', lambda: run.returncode, 0)
            attempt(failures, name+' CLI JSON', lambda: json.loads(run.stdout), result)
        for name, extra in [('missing', []), ('aging', []), ('aging', ['--as-of','bad'])]:
            run = subprocess.run([sys.executable,str(root/'cli.py'),name,str(path),*extra],capture_output=True,text=True,env=env,timeout=10)
            attempt(failures, 'CLI error status', lambda: run.returncode, 2)
            attempt(failures, 'CLI error output', lambda: (run.stdout, bool(run.stderr)), ('',True))
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
