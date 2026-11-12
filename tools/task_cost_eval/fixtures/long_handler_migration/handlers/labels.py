from framework import register

UNIT = 4


def _ok(body):
    return {'status': 200, 'body': body}


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'labels', 'total': request.get('qty', 1) * UNIT}
    return _ok(body)


def _create(request):
    item_id = request.get('id', 0)
    return {'status': 201, 'body': {'id': item_id, 'created': True}, 'headers': {'Location': f'/labels/{item_id}'}}


register('labels.get', _get)
register('labels.create', _create, methods=('POST',))
