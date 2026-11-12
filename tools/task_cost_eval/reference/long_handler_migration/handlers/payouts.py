from framework import Response, route

UNIT = 34


@route('payouts.get')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'payouts', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body)


@route('payouts.create', methods=('POST',))
def _create(request):
    item_id = request.get('id', 0)
    return Response(201, {'id': item_id, 'created': True}, {'Location': f'/payouts/{item_id}'})
