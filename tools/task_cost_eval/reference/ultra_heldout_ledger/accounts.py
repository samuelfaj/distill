from journal import entries


def postings(ledger, event):
    if event['kind'] == 'credit':
        return [{'account':event['account'],'delta':event['amount']}]
    if event['kind'] == 'transfer':
        source,target,amount = event['source'],event['target'],event['amount']
        if source not in ledger.balances or target not in ledger.balances:
            raise ValueError('unknown transfer account')
    else:
        original = ledger.seen.get(event['ref'])
        if original is None or original['kind'] != 'transfer' or event['ref'] in ledger.reversed:
            raise ValueError('transfer cannot be reversed')
        source,target,amount = original['target'],original['source'],original['amount']
    return [{'account':source,'delta':-amount},{'account':target,'delta':amount}]


def snapshot(ledger):
    return {'balances':dict(ledger.balances),'journal':entries(ledger),'last_seq':ledger.last_seq}
