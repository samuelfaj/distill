from framework import Response, route

UNIT = 12


@route('plans.get')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'plans', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body)


@route('plans.create', methods=('POST',))
def _create(request):
    item_id = request.get('id', 0)
    return Response(201, {'id': item_id, 'created': True}, {'Location': f'/plans/{item_id}'})
