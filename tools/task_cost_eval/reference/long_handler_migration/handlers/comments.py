from framework import Response, route

UNIT = 14


@route('comments.get')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'comments', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body, {'Cache-Control': 'max-age=14'})


@route('comments.create', methods=('POST',))
def _create(request):
    item_id = request.get('id', 0)
    return Response(201, {'id': item_id, 'created': True}, {'Location': f'/comments/{item_id}'})
