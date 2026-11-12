from framework import Response, route

UNIT = 4


def _ok(body):
    return Response(200, body)


@route('quotes.get')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'quotes', 'total': request.get('qty', 1) * UNIT}
    return _ok(body)


@route('quotes.create', methods=('POST',))
def _create(request):
    item_id = request.get('id', 0)
    return Response(201, {'id': item_id, 'created': True}, {'Location': f'/quotes/{item_id}'})
