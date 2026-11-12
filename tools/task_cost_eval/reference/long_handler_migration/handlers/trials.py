from framework import Response, route

UNIT = 3


@route('trials.get')
def _get(request):
    if 'id' not in request:
        return Response(404, {'error': 'missing id'})
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'trials', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body)


@route('trials.create', methods=('POST',))
def _create(request):
    item_id = request.get('id', 0)
    return Response(201, {'id': item_id, 'created': True}, {'Location': f'/trials/{item_id}'})
