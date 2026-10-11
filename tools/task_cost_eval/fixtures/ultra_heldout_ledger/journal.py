def record(ledger, event, postings):
    ledger.journal.append({'event_id':event['id'],'seq':event['seq'],'kind':event['kind'],'postings':postings})


def entries(ledger):
    return ledger.journal
