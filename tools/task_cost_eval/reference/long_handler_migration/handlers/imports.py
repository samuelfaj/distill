from framework import Response, route

UNIT = 34


@route('imports.get')
@route('imports.fetch')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'imports', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body)
