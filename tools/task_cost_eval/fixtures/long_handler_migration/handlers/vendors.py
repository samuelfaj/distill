from framework import register

UNIT = 26


def _ok(body):
    return {'status': 200, 'body': body}


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'vendors', 'total': request.get('qty', 1) * UNIT}
    return _ok(body)


register('vendors.get', _get)
register('vendors.fetch', _get)
