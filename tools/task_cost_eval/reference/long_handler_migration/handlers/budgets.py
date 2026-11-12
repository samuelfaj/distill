from framework import Response, route

UNIT = 26


@route('budgets.get')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'budgets', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body, {'Cache-Control': 'max-age=26'})


@route('budgets.create', methods=('POST',))
def _create(request):
    item_id = request.get('id', 0)
    return Response(201, {'id': item_id, 'created': True}, {'Location': f'/budgets/{item_id}'})
