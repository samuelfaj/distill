from framework import Response, route

UNIT = 33


@route('shipments.get')
@route('shipments.fetch')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'shipments', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body)
