from events import normalize
from accounts import postings
from journal import record


def apply(ledger, event):
    event = normalize(event)
    if event['seq'] <= ledger.last_seq:
        raise ValueError('sequence must increase')
    if event['id'] in ledger.seen:
        return False
    changes = postings(ledger,event)
    for change in changes:
        account = change['account']
        ledger.balances[account] = ledger.balances.get(account,0) + change['delta']
        if ledger.balances[account] < 0:
            raise ValueError('insufficient funds')
    record(ledger,event,changes)
    ledger.seen[event['id']] = event
    ledger.last_seq = event['seq']
    if event['kind'] == 'reverse':
        ledger.reversed.add(event['ref'])
    return True
