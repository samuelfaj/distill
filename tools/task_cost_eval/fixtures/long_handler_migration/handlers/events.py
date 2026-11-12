from framework import register

UNIT = 12


def _ok(body):
    return {'status': 200, 'body': body}


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'events', 'total': request.get('qty', 1) * UNIT}
    return _ok(body)


register('events.get', _get)
register('events.fetch', _get)
