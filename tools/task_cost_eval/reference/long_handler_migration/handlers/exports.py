from framework import Response, route

UNIT = 5


def _ok(body):
    return Response(200, body)


@route('exports.get')
def _get(request):
    if 'id' not in request:
        return Response(404, {'error': 'missing id'})
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'exports', 'total': request.get('qty', 1) * UNIT}
    return _ok(body)
