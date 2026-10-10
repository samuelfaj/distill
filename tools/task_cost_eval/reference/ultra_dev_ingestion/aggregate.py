from errors import FeedError


def summarize(records):
    seen, totals = {}, {}
    for row, record in enumerate(records, 1):
        if record['id'] in seen:
            if record != seen[record['id']]:
                raise FeedError('conflict', row, 'id')
            continue
        seen[record['id']] = record
        key = record['warehouse'], record['sku']
        totals[key] = totals.get(key, 0) + record['delta']
    return [{'warehouse':w, 'sku':s, 'delta':totals[(w,s)]} for w,s in sorted(totals)]
