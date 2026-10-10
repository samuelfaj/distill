def normalize(event):
    if not isinstance(event,dict):
        raise ValueError('event must be an object')
    identifier, seq, kind = event.get('id'),event.get('seq'),event.get('kind')
    if not isinstance(identifier,str) or not identifier or type(seq) is not int or seq <= 0:
        raise ValueError('invalid event identity')
    result = {'id':identifier,'seq':seq,'kind':kind}
    fields = {'credit':('account','amount'),'transfer':('source','target','amount'),'reverse':('ref',)}
    if kind not in fields:
        raise ValueError('unknown event kind')
    for field in fields[kind]:
        value = event.get(field)
        if field == 'amount':
            if type(value) is not int or value <= 0:
                raise ValueError('invalid amount')
        elif not isinstance(value,str) or not value:
            raise ValueError('invalid account/reference')
        result[field] = value
    if kind == 'transfer' and result['source'] == result['target']:
        raise ValueError('transfer accounts must differ')
    return result
