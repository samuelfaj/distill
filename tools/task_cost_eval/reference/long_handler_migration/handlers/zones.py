from framework import Response, route

UNIT = 6


def _ok(body):
    return Response(200, body)


@route('zones.get')
@route('zones.fetch')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'zones', 'total': request.get('qty', 1) * UNIT}
    return _ok(body)
