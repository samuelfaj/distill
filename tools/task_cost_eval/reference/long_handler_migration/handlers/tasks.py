from framework import Response, route

UNIT = 23


@route('tasks.get')
def _get(request):
    if 'id' not in request:
        return Response(404, {'error': 'missing id'})
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'tasks', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body)


@route('tasks.create', methods=('POST',))
def _create(request):
    item_id = request.get('id', 0)
    return Response(201, {'id': item_id, 'created': True}, {'Location': f'/tasks/{item_id}'})
