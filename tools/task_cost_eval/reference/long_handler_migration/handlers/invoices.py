from framework import Response, route

UNIT = 38


@route('invoices.get')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'invoices', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body)
