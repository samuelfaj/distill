#!/usr/bin/env python3
"""Synthetic stdio MCP server for the realistic benchmark profile.

Newline-delimited JSON-RPC 2.0 with two read-only tools over static data, so it
adds the tool definitions a user's MCP server would add without touching files,
the network or the clock.
"""
import json
import sys

GLOSSARY = {
    'backorder': 'An order line accepted while stock is zero; it ships when stock arrives.',
    'sku': 'Stock keeping unit: the identifier of one sellable item variant.',
    'settlement': 'The daily batch that moves captured payments to the merchant balance.',
    'tombstone': 'A record kept after deletion so replicas learn the item was removed.',
}
TOOLS = [
    {'name': 'lookup_glossary',
     'description': 'Look up a term in the team glossary.',
     'inputSchema': {'type': 'object', 'properties': {'term': {'type': 'string'}}, 'required': ['term']}},
    {'name': 'count_words',
     'description': 'Count the words in a piece of text.',
     'inputSchema': {'type': 'object', 'properties': {'text': {'type': 'string'}}, 'required': ['text']}},
]


def call_tool(name, arguments):
    if name == 'lookup_glossary':
        term = str(arguments.get('term', '')).strip().lower()
        return GLOSSARY.get(term, f'No glossary entry for "{term}".'), False
    if name == 'count_words':
        return str(len(str(arguments.get('text', '')).split())), False
    return f'Unknown tool: {name}', True


def respond(request):
    method, params = request.get('method'), request.get('params') or {}
    if method == 'initialize':
        return {'protocolVersion': params.get('protocolVersion', '2024-11-05'),
                'capabilities': {'tools': {}}, 'serverInfo': {'name': 'team-notes', 'version': '1.0.0'}}
    if method == 'ping':
        return {}
    if method == 'tools/list':
        return {'tools': TOOLS}
    if method == 'tools/call':
        text, failed = call_tool(params.get('name'), params.get('arguments') or {})
        return {'content': [{'type': 'text', 'text': text}], 'isError': failed}
    raise LookupError(method)


def main():
    for line in sys.stdin:
        if not line.strip():
            continue
        request = json.loads(line)
        if 'id' not in request:
            continue  # notifications such as notifications/initialized need no reply
        try:
            reply = {'jsonrpc': '2.0', 'id': request['id'], 'result': respond(request)}
        except LookupError as error:
            reply = {'jsonrpc': '2.0', 'id': request['id'],
                     'error': {'code': -32601, 'message': f'Method not found: {error}'}}
        print(json.dumps(reply), flush=True)


if __name__ == '__main__':
    main()
