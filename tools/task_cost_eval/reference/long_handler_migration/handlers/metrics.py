from framework import Response, route

UNIT = 36


@route('metrics.get')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'metrics', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body, {'Cache-Control': 'max-age=36'})


@route('metrics.create', methods=('POST',))
def _create(request):
    item_id = request.get('id', 0)
    return Response(201, {'id': item_id, 'created': True}, {'Location': f'/metrics/{item_id}'})
