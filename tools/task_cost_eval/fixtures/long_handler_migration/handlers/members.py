from framework import register

UNIT = 4


def _ok(body):
    return {'status': 200, 'body': body}


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'members', 'total': request.get('qty', 1) * UNIT}
    return _ok(body)


register('members.get', _get)
register('members.fetch', _get)
