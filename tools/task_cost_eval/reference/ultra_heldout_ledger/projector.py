from events import normalize
from accounts import postings
from journal import record


def apply(ledger, event):
    event = normalize(event)
    if event['id'] in ledger.seen:
        if ledger.seen[event['id']] != event:
            raise ValueError('conflicting event id')
        return False
    if event['seq'] <= ledger.last_seq:
        raise ValueError('sequence must increase')
    changes = postings(ledger,event)
    balances = dict(ledger.balances)
    for change in changes:
        account = change['account']
        balances[account] = balances.get(account,0) + change['delta']
        if balances[account] < 0:
            raise ValueError('insufficient funds')
    ledger.balances = balances
    record(ledger,event,changes)
    ledger.seen[event['id']] = event
    ledger.last_seq = event['seq']
    if event['kind'] == 'reverse':
        ledger.reversed.add(event['ref'])
    return True
