from framework import register

UNIT = 23


def _get(request):
    if 'id' not in request:
        return 404, {'error': 'missing id'}
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'tasks', 'total': request.get('qty', 1) * UNIT}
    return 200, body


def _create(request):
    item_id = request.get('id', 0)
    return {'status': 201, 'body': {'id': item_id, 'created': True}, 'headers': {'Location': f'/tasks/{item_id}'}}


register('tasks.get', _get)
register('tasks.create', _create, methods=('POST',))
