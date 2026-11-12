from framework import register

UNIT = 37


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'rooms', 'total': request.get('qty', 1) * UNIT}
    return {'status': 200, 'body': body, 'headers': {'Cache-Control': 'max-age=37'}}


def _create(request):
    item_id = request.get('id', 0)
    return {'status': 201, 'body': {'id': item_id, 'created': True}, 'headers': {'Location': f'/rooms/{item_id}'}}


register('rooms.get', _get)
register('rooms.create', _create, methods=('POST',))
