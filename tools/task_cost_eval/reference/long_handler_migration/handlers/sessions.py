from framework import Response, route

UNIT = 23


@route('sessions.get')
@route('sessions.fetch')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'sessions', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body)
