from framework import Response, route

UNIT = 6


@route('venues.get')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'venues', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body)


@route('venues.create', methods=('POST',))
def _create(request):
    item_id = request.get('id', 0)
    return Response(201, {'id': item_id, 'created': True}, {'Location': f'/venues/{item_id}'})
