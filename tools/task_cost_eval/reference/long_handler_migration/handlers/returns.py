from framework import Response, route

UNIT = 19


@route('returns.get')
@route('returns.fetch')
def _get(request):
    if 'id' not in request:
        return Response(404, {'error': 'missing id'})
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'returns', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body, {'Cache-Control': 'max-age=19'})
