from framework import Response, route

UNIT = 2


@route('uploads.get')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'uploads', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body, {'Cache-Control': 'max-age=2'})
