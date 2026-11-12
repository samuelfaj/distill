from framework import Response, route

UNIT = 7


def _ok(body):
    return Response(200, body)


@route('guests.get')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'guests', 'total': request.get('qty', 1) * UNIT}
    return _ok(body)


@route('guests.create', methods=('POST',))
def _create(request):
    item_id = request.get('id', 0)
    return Response(201, {'id': item_id, 'created': True}, {'Location': f'/guests/{item_id}'})
