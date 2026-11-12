from framework import register

UNIT = 23


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'sessions', 'total': request.get('qty', 1) * UNIT}
    return {'status': 200, 'body': body}


register('sessions.get', _get)
register('sessions.fetch', _get)
