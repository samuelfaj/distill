from framework import register

UNIT = 22


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'tokens', 'total': request.get('qty', 1) * UNIT}
    return 200, body


register('tokens.get', _get)
register('tokens.fetch', _get)
