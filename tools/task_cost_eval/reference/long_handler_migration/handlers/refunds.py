from framework import Response, route

UNIT = 26


@route('refunds.get')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'refunds', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body, {'Cache-Control': 'max-age=26'})
