from framework import Response, route

UNIT = 14


@route('bookings.get')
def _get(request):
    if 'id' not in request:
        return Response(404, {'error': 'missing id'})
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'bookings', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body)


@route('bookings.create', methods=('POST',))
def _create(request):
    item_id = request.get('id', 0)
    return Response(201, {'id': item_id, 'created': True}, {'Location': f'/bookings/{item_id}'})
