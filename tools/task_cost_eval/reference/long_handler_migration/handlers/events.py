from framework import Response, route

UNIT = 12


def _ok(body):
    return Response(200, body)


@route('events.get')
@route('events.fetch')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'events', 'total': request.get('qty', 1) * UNIT}
    return _ok(body)
