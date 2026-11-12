from framework import Response, route

UNIT = 9


@route('dishes.get')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'dishes', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body)
