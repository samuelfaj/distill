def summarize(records):
    totals = {}
    for record in records:
        key = record['warehouse'], record['sku']
        totals[key] = totals.get(key, 0) + record['delta']
    return [{'warehouse':w, 'sku':s, 'delta':totals[(w,s)]} for w,s in sorted(totals)]
