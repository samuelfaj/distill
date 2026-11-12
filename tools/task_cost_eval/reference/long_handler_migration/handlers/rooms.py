from framework import Response, route

UNIT = 37


@route('rooms.get')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'rooms', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body, {'Cache-Control': 'max-age=37'})


@route('rooms.create', methods=('POST',))
def _create(request):
    item_id = request.get('id', 0)
    return Response(201, {'id': item_id, 'created': True}, {'Location': f'/rooms/{item_id}'})
