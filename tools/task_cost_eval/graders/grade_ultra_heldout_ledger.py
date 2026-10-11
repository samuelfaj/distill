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
    root = fixture_dir(argv,'ultra_heldout_ledger')
    failures = []
    try:
        events, state, journal, accounts, projector, replay = import_fresh(root,'events','state','journal','accounts','projector','replay')
    except Exception as error:
        return finish(['import: '+repr(error)])
    initial = {'a':100,'b':0,'c':0}
    ledger = state.Ledger(initial)
    initial['a'] = 1000
    attempt(failures,'owned initial state',lambda: accounts.snapshot(ledger)['balances'],{'a':100,'b':0,'c':0})
    t = {'id':'transfer','seq':2,'kind':'transfer','source':'a','target':'b','amount':30}
    credit = {'id':'credit','seq':4,'kind':'credit','account':'c','amount':7}
    reverse = {'id':'reverse','seq':8,'kind':'reverse','ref':'transfer'}
    attempt(failures,'accept transfer gap',lambda: projector.apply(ledger,t),True)
    attempt(failures,'accept credit gap',lambda: projector.apply(ledger,credit),True)
    attempt(failures,'old retry after later delivery',lambda: projector.apply(ledger,{**t,'ignored_metadata':'anything'}),False)
    attempt(failures,'conflicting accepted id',lambda: projector.apply(ledger,{**t,'seq':6,'amount':1}),raises=ValueError)
    expected_mid = {'balances':{'a':70,'b':30,'c':7},'last_seq':4,'journal':[
        {'event_id':'transfer','seq':2,'kind':'transfer','postings':[{'account':'a','delta':-30},{'account':'b','delta':30}]},
        {'event_id':'credit','seq':4,'kind':'credit','postings':[{'account':'c','delta':7}]}]}
    attempt(failures,'one journal per event',lambda: accounts.snapshot(ledger),expected_mid)
    attempt(failures,'out of order unseen',lambda: projector.apply(ledger,{'id':'late','seq':3,'kind':'credit','account':'a','amount':2}),raises=ValueError)
    attempt(failures,'reverse transfer',lambda: projector.apply(ledger,reverse),True)
    attempt(failures,'reverse retry',lambda: projector.apply(ledger,reverse),False)
    attempt(failures,'transfer retry after reversal',lambda: projector.apply(ledger,t),False)
    expected = {'balances':{'a':100,'b':0,'c':7},'last_seq':8,'journal':expected_mid['journal']+[
        {'event_id':'reverse','seq':8,'kind':'reverse','postings':[{'account':'b','delta':-30},{'account':'a','delta':30}]}]}
    attempt(failures,'reverse accounting',lambda: accounts.snapshot(ledger),expected)
    for ref in ('transfer','credit','reverse','missing'):
        attempt(failures,'invalid reversal atomic',lambda: projector.apply(ledger,{'id':'bad-reversal','seq':9,'kind':'reverse','ref':ref}),raises=ValueError)
        attempt(failures,'reversal failure snapshot',lambda: accounts.snapshot(ledger),expected)
    def detached():
        snap = accounts.snapshot(ledger)
        snap['balances']['a'] = -100
        snap['journal'][0]['postings'][0]['delta'] = 999
        snap['journal'].clear()
        direct = journal.entries(ledger)
        direct[0]['postings'].clear()
        return accounts.snapshot(ledger)
    attempt(failures,'nested snapshots',detached,expected)
    def transaction_recovery():
        local = state.Ledger({'a':5,'b':0})
        failed = {'id':'retryable','seq':1,'kind':'transfer','source':'a','target':'b','amount':9}
        before = copy.deepcopy(accounts.snapshot(local))
        try:
            projector.apply(local,failed)
        except ValueError:
            pass
        else:
            raise AssertionError('expected insufficient funds')
        if accounts.snapshot(local) != before:
            raise AssertionError('rejected transfer changed state')
        accepted = projector.apply(local,{**failed,'amount':3})
        return accepted,accounts.snapshot(local)['balances']
    attempt(failures,'failed id and seq reusable',transaction_recovery,(True,{'a':2,'b':3}))
    def failed_reverse_recovery():
        local = state.Ledger({'a':10,'b':0,'c':0})
        projector.apply(local,{'id':'t','seq':1,'kind':'transfer','source':'a','target':'b','amount':8})
        projector.apply(local,{'id':'spent','seq':2,'kind':'transfer','source':'b','target':'c','amount':8})
        rejected = {'id':'r','seq':3,'kind':'reverse','ref':'t'}
        before = copy.deepcopy(accounts.snapshot(local))
        try:
            projector.apply(local,rejected)
        except ValueError:
            pass
        else:
            raise AssertionError('expected reversal funds error')
        if accounts.snapshot(local) != before:
            raise AssertionError('failed reversal changed state')
        projector.apply(local,{'id':'fund','seq':3,'kind':'credit','account':'b','amount':8})
        projector.apply(local,{**rejected,'seq':4})
        return accounts.snapshot(local)['balances']
    attempt(failures,'failed reversal eligibility preserved',failed_reverse_recovery,{'a':10,'b':0,'c':8})
    deliveries = [t,credit,t,reverse,t,reverse]
    clean_initial = {'a':100,'b':0,'c':0}
    saved = copy.deepcopy(deliveries)
    attempt(failures,'stream replay integration',lambda: replay.replay(clean_initial,deliveries),expected)
    attempt(failures,'repeat replay independent',lambda: replay.replay(clean_initial,deliveries),expected)
    attempt(failures,'input events unchanged',lambda: deliveries,saved)
    attempt(failures,'input balances unchanged',lambda: clean_initial,{'a':100,'b':0,'c':0})
    attempt(failures,'empty replay',lambda: replay.replay({},[]),{'balances':{},'journal':[],'last_seq':0})
    attempt(failures,'create account credit',lambda: replay.replay({},[{'id':'new','seq':1,'kind':'credit','account':'new','amount':10**18}])['balances'],{'new':10**18})
    for invalid in ({'id':'bad','seq':True,'kind':'credit','account':'a','amount':2},{'id':'bad','seq':9,'kind':'credit','account':'a','amount':True},{'id':'bad','seq':9,'kind':'transfer','source':'a','target':'a','amount':1}):
        attempt(failures,'event validation',lambda: projector.apply(ledger,invalid),raises=ValueError)
    attempt(failures,'validation no mutation',lambda: accounts.snapshot(ledger),expected)
    attempt(failures,'unknown account',lambda: projector.apply(ledger,{'id':'unknown','seq':9,'kind':'transfer','source':'a','target':'missing','amount':1}),raises=ValueError)
    attempt(failures,'unknown account atomic',lambda: accounts.snapshot(ledger),expected)
    for amount in (-1,True,1.2):
        attempt(failures,'initial validation',lambda: state.Ledger({'a':amount}),raises=ValueError)
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp)/'events.json'
        path.write_text(json.dumps({'initial':clean_initial,'events':deliveries}))
        env = {**os.environ,'PYTHONDONTWRITEBYTECODE':'1'}
        run = subprocess.run([sys.executable,str(root/'cli.py'),str(path)],capture_output=True,text=True,env=env,timeout=10)
        attempt(failures,'CLI status',lambda: run.returncode,0)
        attempt(failures,'CLI replay',lambda: json.loads(run.stdout),expected)
        path.write_text(json.dumps({'initial':{'a':0,'b':0},'events':[t]}))
        run = subprocess.run([sys.executable,str(root/'cli.py'),str(path)],capture_output=True,text=True,env=env,timeout=10)
        attempt(failures,'CLI failure',lambda: (run.returncode,run.stdout,bool(run.stderr)),(2,'',True))
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
