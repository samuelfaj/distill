from framework import register

UNIT = 6


def _ok(body):
    return {'status': 200, 'body': body}


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'zones', 'total': request.get('qty', 1) * UNIT}
    return _ok(body)


register('zones.get', _get)
register('zones.fetch', _get)
