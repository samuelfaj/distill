from framework import Response, route

UNIT = 10


@route('coupons.get')
@route('coupons.fetch')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'coupons', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body)
