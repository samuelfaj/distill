from framework import register

UNIT = 33


def _ok(body):
    return {'status': 200, 'body': body}


def _get(request):
    if 'id' not in request:
        return {'status': 404, 'body': {'error': 'missing id'}}
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'carts', 'total': request.get('qty', 1) * UNIT}
    return _ok(body)


register('carts.get', _get)
