from framework import Response, route

UNIT = 35


def _ok(body):
    return Response(200, body)


@route('batches.get')
def _get(request):
    if 'id' not in request:
        return Response(404, {'error': 'missing id'})
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'batches', 'total': request.get('qty', 1) * UNIT}
    return _ok(body)
