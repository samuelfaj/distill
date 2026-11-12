from framework import Response, route

UNIT = 18


@route('drivers.get')
@route('drivers.fetch')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'drivers', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body)
