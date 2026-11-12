from framework import register

UNIT = 7


def _ok(body):
    return {'status': 200, 'body': body}


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'guests', 'total': request.get('qty', 1) * UNIT}
    return _ok(body)


def _create(request):
    item_id = request.get('id', 0)
    return {'status': 201, 'body': {'id': item_id, 'created': True}, 'headers': {'Location': f'/guests/{item_id}'}}


register('guests.get', _get)
register('guests.create', _create, methods=('POST',))
