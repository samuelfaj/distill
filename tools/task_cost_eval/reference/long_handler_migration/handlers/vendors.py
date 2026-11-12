from framework import Response, route

UNIT = 26


def _ok(body):
    return Response(200, body)


@route('vendors.get')
@route('vendors.fetch')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'vendors', 'total': request.get('qty', 1) * UNIT}
    return _ok(body)
