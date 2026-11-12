from framework import Response, route

UNIT = 31


@route('routes.get')
@route('routes.fetch')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'routes', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body)
