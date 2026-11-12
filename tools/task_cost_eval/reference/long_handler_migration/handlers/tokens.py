from framework import Response, route

UNIT = 22


@route('tokens.get')
@route('tokens.fetch')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'tokens', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body)
