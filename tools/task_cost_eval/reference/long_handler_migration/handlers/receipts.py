from framework import Response, route

UNIT = 24


@route('receipts.get')
def _get(request):
    if 'id' not in request:
        return Response(404, {'error': 'missing id'})
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'receipts', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body)
