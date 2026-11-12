from framework import Response, route

UNIT = 35


def _ok(body):
    return Response(200, body)


@route('surveys.get')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'surveys', 'total': request.get('qty', 1) * UNIT}
    return _ok(body)


@route('surveys.create', methods=('POST',))
def _create(request):
    item_id = request.get('id', 0)
    return Response(201, {'id': item_id, 'created': True}, {'Location': f'/surveys/{item_id}'})
